import { defineConfig, devices } from "@playwright/test";

const PORT = Number(process.env.GAMJA_PORT || 4175);

export default defineConfig({
	testDir: "e2e-real",
	testMatch: "*.spec.ts",
	fullyParallel: false,
	workers: 1,
	timeout: 60_000,
	reporter: "list",
	use: {
		baseURL: process.env.BASE_URL || `http://localhost:${PORT}`,
		trace: "retain-on-failure",
		screenshot: "only-on-failure",
	},
	projects: [
		{
			name: "chromium",
			use: { ...devices["Desktop Chrome"], launchOptions: { executablePath: "/usr/bin/chromium" } },
		},
	],
	webServer: process.env.BASE_URL ? undefined : {
		command: `python3 -m http.server ${PORT} --bind 127.0.0.1 --directory dist`,
		url: `http://localhost:${PORT}`,
		reuseExistingServer: true,
		timeout: 60_000,
	},
});
