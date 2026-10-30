// End-to-end tests of gamja against a real soju (Go or Rust) bouncer, with
// an ergo IRC server upstream. Configured by environment variables:
//   SOJU_WS    WebSocket URL of the bouncer (ws://127.0.0.1:16671/socket)
//   ERGO_PORT  TCP port of the upstream ergo server (bots connect there)
import { test, expect, type Page, type Browser } from "@playwright/test";
import net from "node:net";

const SOJU_WS = process.env.SOJU_WS || "ws://127.0.0.1:16671/socket";
const ERGO_PORT = Number(process.env.ERGO_PORT || 16667);

/** A scripted IRC user connected directly to ergo. */
class Bot {
	sock: net.Socket;
	nick: string;
	received: string[] = [];
	private buf = "";
	private waiters: { pred: (l: string) => boolean; resolve: (l: string) => void }[] = [];

	constructor(nick: string) {
		this.nick = nick;
		this.sock = net.connect(ERGO_PORT, "127.0.0.1");
		this.sock.setEncoding("utf8");
		this.sock.on("data", (d: string) => {
			this.buf += d;
			let i;
			while ((i = this.buf.indexOf("\n")) >= 0) {
				const line = this.buf.slice(0, i).replace(/\r$/, "");
				this.buf = this.buf.slice(i + 1);
				this.received.push(line);
				if (line.startsWith("PING ")) {
					this.send("PONG " + line.slice(5));
				}
				this.waiters = this.waiters.filter((w) => {
					if (w.pred(line)) {
						w.resolve(line);
						return false;
					}
					return true;
				});
			}
		});
	}

	static async connect(nick: string): Promise<Bot> {
		const bot = new Bot(nick);
		bot.send(`NICK ${nick}`);
		bot.send(`USER ${nick} 0 * :${nick} bot`);
		await bot.waitFor((l) => / 376 | 422 /.test(l));
		return bot;
	}

	send(line: string) {
		this.sock.write(line + "\r\n");
	}

	waitFor(pred: (l: string) => boolean, timeout = 10000): Promise<string> {
		const found = this.received.find(pred);
		if (found) return Promise.resolve(found);
		return new Promise((resolve, reject) => {
			const t = setTimeout(() => reject(new Error(`${this.nick}: timed out`)), timeout);
			this.waiters.push({
				pred,
				resolve: (l) => {
					clearTimeout(t);
					resolve(l);
				},
			});
		});
	}

	async join(channel: string) {
		this.send(`JOIN ${channel}`);
		await this.waitFor((l) => l.includes(` 366 ${this.nick} ${channel} `));
	}

	privmsg(target: string, text: string) {
		this.send(`PRIVMSG ${target} :${text}`);
	}

	close() {
		this.sock.destroy();
	}
}

const tabs = (page: Page) => page.getByRole("tablist", { name: "Buffer list" });
const composer = (page: Page) => page.getByRole("textbox", { name: /Type a/ });

async function login(page: Page, nick = "alice", password = "secret") {
	await page.goto("/?" + new URLSearchParams({ server: SOJU_WS }).toString());
	await page.getByLabel("Nickname").fill(nick);
	await page.getByLabel(/^Password:?$/).fill(password);
	await page.getByRole("button", { name: "Connect" }).click();
	await expect(tabs(page).getByRole("tab", { name: "bouncer" }).first()).toBeVisible();
}

async function addNetwork(page: Page, name: string) {
	await tabs(page).getByRole("tab", { name: "bouncer" }).first().click();
	await page.getByRole("button", { name: "Add network" }).first().click();
	const dialog = page.getByRole("dialog", { name: "Add network" });
	await dialog.getByLabel("Hostname").fill("127.0.0.1");
	await dialog.getByText("Advanced options").click();
	await dialog.getByLabel("Port").fill(String(ERGO_PORT));
	const tlsBox = dialog.getByLabel(/TLS/);
	if (await tlsBox.isChecked()) {
		await tlsBox.uncheck();
	}
	await dialog.getByLabel("Network name").fill(name);
	await dialog.getByRole("button", { name: "Add network" }).click();
	await expect(tabs(page).getByRole("tab", { name })).toBeVisible();
}

async function openNet(page: Page, name: string) {
	await tabs(page).getByRole("tab", { name }).click();
	await expect(page.locator("#composer")).not.toHaveClass(/read-only/, { timeout: 15000 });
}

async function run(page: Page, command: string) {
	await composer(page).fill(command);
	await page.keyboard.press("Enter");
}

async function join(page: Page, net: string, channel: string) {
	await openNet(page, net);
	await run(page, `/join ${channel}`);
	await expect(page.locator("#buffer-header h1")).toHaveText(channel);
}

test.describe.configure({ mode: "serial" });

test("adds a network, joins, chats both ways and edits the network", async ({ page }) => {
	await login(page);
	await addNetwork(page, "ergo");
	// alice joins first, to be a channel operator
	await join(page, "ergo", "#gamja");
	const bob = await Bot.connect("bob");
	await bob.join("#gamja");

	bob.privmsg("#gamja", "hello from bob");
	await expect(page.locator("#buffer")).toContainText("hello from bob");

	await run(page, "hello from alice");
	await bob.waitFor((l) => l.includes("PRIVMSG #gamja :hello from alice"));
	await expect(page.locator("#buffer")).toContainText("hello from alice");

	// Private message opens a query buffer
	bob.privmsg("alice", "psst alice");
	await expect(tabs(page).getByRole("tab", { name: "bob" })).toBeVisible();

	// Topic
	await run(page, "/topic a topic from gamja");
	await bob.waitFor((l) => l.includes("TOPIC #gamja :a topic from gamja"));
	await expect(page.locator("#buffer-header")).toContainText("a topic from gamja");

	// Rename the network
	await tabs(page).getByRole("tab", { name: "ergo" }).click();
	await page.getByRole("button", { name: "Manage network" }).click();
	const edit = page.getByRole("dialog", { name: "Edit network" });
	await edit.getByText("Advanced options").click();
	await edit.getByLabel("Network name").fill("ergo2");
	await edit.getByRole("button", { name: "Save network" }).click();
	await expect(tabs(page).getByRole("tab", { name: "ergo2" })).toBeVisible();
	await openNet(page, "ergo2");
	bob.close();
});

test("loads history, keeps read markers in sync and searches", async ({ browser }) => {
	const page = await browser.newPage();
	await login(page);
	await expect(tabs(page).getByRole("tab", { name: "ergo2" })).toBeVisible();
	await expect(tabs(page).getByRole("tab", { name: "#gamja" })).toBeVisible({ timeout: 15000 });
	await tabs(page).getByRole("tab", { name: "#gamja" }).click();
	// History from the previous session, fetched with CHATHISTORY
	await expect(page.locator("#buffer")).toContainText("hello from bob");
	await expect(page.locator("#buffer")).toContainText("hello from alice");

	const bob = await Bot.connect("bob");
	await bob.join("#gamja");
	bob.privmsg("#gamja", "the quick brown fox");
	bob.privmsg("#gamja", "jumps over the lazy dog");
	await expect(page.locator("#buffer")).toContainText("lazy dog");

	// A second client sees #gamja as read since the first one is looking at it
	const page2 = await browser.newPage();
	await login(page2);
	const tab2 = tabs(page2).getByRole("tab", { name: "#gamja" });
	await expect(tab2).toBeVisible({ timeout: 15000 });
	await expect(tab2.locator("xpath=..")).not.toHaveClass(/unread/, { timeout: 10000 });

	// Unread marker for messages received while looking at another buffer
	await tabs(page).getByRole("tab", { name: "ergo2" }).click();
	await tabs(page2).getByRole("tab", { name: "ergo2" }).click();
	bob.privmsg("#gamja", "unread message");
	await expect(tab2.locator("xpath=..")).toHaveClass(/unread/, { timeout: 10000 });
	await tabs(page).getByRole("tab", { name: "#gamja" }).click();
	// Reading on the first client clears the unread state on the second
	await expect(tab2.locator("xpath=..")).not.toHaveClass(/unread/, { timeout: 10000 });

	// Search
	await page.getByRole("button", { name: "Search" }).click();
	const dialog = page.getByRole("dialog", { name: /Search/ });
	await dialog.getByRole("searchbox").fill("brown");
	await dialog.getByRole("searchbox").press("Enter");
	await expect(dialog.getByRole("status")).toHaveText("1 message found");
	await dialog.getByRole("button", { name: /quick brown fox/ }).click();
	await expect(dialog).toBeHidden();
	await expect(page.locator("#buffer .logline.flash")).toContainText("quick brown fox");
	bob.close();
	await page.close();
	await page2.close();
});

test("pins, mutes and detaches channels", async ({ page }) => {
	await login(page);
	await expect(tabs(page).getByRole("tab", { name: "#gamja" })).toBeVisible({ timeout: 15000 });
	await join(page, "ergo2", "#aaa");
	await tabs(page).getByRole("tab", { name: "#gamja" }).click();
	const names = async () => (await tabs(page).getByRole("tab").allInnerTexts()).filter((n) => n.startsWith("#"));
	await page.getByRole("button", { name: "Pin" }).click();
	await expect(page.getByRole("button", { name: "Unpin" })).toBeVisible();
	await expect.poll(names).toEqual(["#gamja", "#aaa"]);
	await page.getByRole("button", { name: "Mute" }).click();
	await expect(tabs(page).getByRole("tab", { name: "#gamja" }).locator("xpath=..")).toHaveClass(/muted/);

	// The pin and mute state is stored by the bouncer
	await page.reload();
	await page.getByLabel("Nickname").fill("alice");
	await page.getByLabel(/^Password:?$/).fill("secret");
	await page.getByRole("button", { name: "Connect" }).click();
	await expect(tabs(page).getByRole("tab", { name: "#gamja" })).toBeVisible({ timeout: 15000 });
	await expect.poll(names).toEqual(["#gamja", "#aaa"]);
	await expect(tabs(page).getByRole("tab", { name: "#gamja" }).locator("xpath=..")).toHaveClass(/muted/);

	// Detach
	await tabs(page).getByRole("tab", { name: "#aaa" }).click();
	await page.getByRole("button", { name: "Detach" }).click();
	await expect(page.getByRole("button", { name: "Join", exact: true })).toBeVisible();
	await expect(tabs(page).getByRole("tab", { name: "BouncerServ" })).toHaveCount(0);
});

test("talks to BouncerServ and removes the network", async ({ page }) => {
	await login(page);
	await tabs(page).getByRole("tab", { name: "bouncer" }).first().click();
	await run(page, "/msg BouncerServ network status");
	await expect(tabs(page).getByRole("tab", { name: "BouncerServ" })).toBeVisible();
	await tabs(page).getByRole("tab", { name: "BouncerServ" }).click();
	await expect(page.locator("#buffer")).toContainText("ergo2 (irc+insecure://127.0.0.1:" + ERGO_PORT + ") [connected]");

	await tabs(page).getByRole("tab", { name: "ergo2" }).click();
	await page.getByRole("button", { name: "Manage network" }).click();
	await page.getByRole("dialog", { name: "Edit network" }).getByRole("button", { name: "Remove network" }).click();
	await expect(tabs(page).getByRole("tab", { name: "ergo2" })).toHaveCount(0);
});
