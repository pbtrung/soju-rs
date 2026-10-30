#!/usr/bin/env python3
"""Exercise two real upstreams and many downstreams; retain logs and RSS samples."""

import argparse
import asyncio
import contextlib
import json
import os
from pathlib import Path
import random
import signal
import statistics
import tempfile
import time


async def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin', type=Path, default=Path('build/gcc-release'))
    parser.add_argument('--compat-work', type=Path, default=Path('build/compat'))
    parser.add_argument('--seconds', type=int, default=600)
    parser.add_argument('--users', type=int, default=200)
    parser.add_argument('--clients', type=int, default=2000)
    parser.add_argument('--base-port', type=int, default=18667)
    parser.add_argument('--max-growth-kib-minute', type=int, default=512)
    args = parser.parse_args()
    assert args.seconds >= 10 and args.clients >= args.users > 0
    binary, compat = args.bin.resolve(), args.compat_work.resolve()
    Path('build').mkdir(exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix='soak-', dir=Path('build').resolve()))
    print(f'logs: {root}', flush=True)
    processes, logs, clients = [], [], []
    tasks, samples = set(), []
    config = root / 'config'
    base, port = args.base_port, args.base_port + 3
    rng = random.Random(42)
    env = dict(os.environ, SOJU_LOOPS=os.environ.get('SOJU_LOOPS', '4'))
    async def start(argv, cwd, name):
        log = (root / f'{name}.log').open('w')
        logs.append(log)
        proc = await asyncio.create_subprocess_exec(*map(str, argv), cwd=cwd, env=env,
                                                   stdout=log, stderr=log)
        processes.append(proc)
        return proc
    async def ctl(*words):
        proc = await asyncio.create_subprocess_exec(str(binary / 'sojuctl'), '-config', str(config),
            *map(str, words), env=env, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        out, err = await asyncio.wait_for(proc.communicate(), 60)
        assert proc.returncode == 0, (words, out, err)
    async def drain(reader, writer, ready):
        try:
            while line := await reader.readline():
                if line.startswith(b'PING '):
                    writer.write(b'PONG ' + line[5:])
                    await writer.drain()
                if b' 001 ' in line:
                    ready.set()
        except (ConnectionError, asyncio.CancelledError):
            pass
    async def connect(index):
        reader, writer = await asyncio.open_connection('127.0.0.1', port)
        ready = asyncio.Event()
        task = asyncio.create_task(drain(reader, writer, ready))
        tasks.add(task)
        task.add_done_callback(tasks.discard)
        user = index % args.users
        writer.write((f'CAP LS 302\r\nCAP REQ :batch server-time draft/chathistory\r\n'
            f'PASS secret\r\nNICK u{user}\r\nUSER u{user}/net@c{index} 0 * :soak\r\nCAP END\r\n').encode())
        await writer.drain()
        try:
            await asyncio.wait_for(ready.wait(), 60)
        except BaseException:
            writer.close()
            task.cancel()
            raise
        return writer
    def sample(elapsed):
        status = Path(f'/proc/{server.pid}/status').read_text()
        rss = int(next(line.split()[1] for line in status.splitlines() if line.startswith('VmRSS:')))
        samples.append((elapsed, rss))
        print(f'{elapsed:6.1f}s rss={rss} KiB clients={len(clients)}', flush=True)
        (root / 'rss.json').write_text(json.dumps(samples))
    try:
        template = (compat / 'env/ergo/ircd.yaml').read_text()
        for offset in range(2):
            directory = root / f'ergo-{offset}'
            directory.mkdir()
            (directory / 'languages').symlink_to(compat / 'src/ergo/languages')
            text = template.replace('127.0.0.1:16667', f'127.0.0.1:{base + offset}')
            text = text.replace('TLSPORT', str(base + offset + 30))
            text = text.replace(str(compat / 'env/ergo/ircd.db'), str(directory / 'ircd.db'))
            (directory / 'ircd.yaml').write_text(text)
            proc = await start([compat / 'bin/ergo', 'initdb', '--conf', 'ircd.yaml'], directory, f'init-{offset}')
            assert await proc.wait() == 0
            await start([compat / 'bin/ergo', 'run', '--conf', 'ircd.yaml'], directory, f'ergo-{offset}')
        config.write_text(f'db sqlite3 {root}/soju.db\nhostname soak.test\n'
            f'listen irc+insecure://127.0.0.1:{port}\nlisten unix+admin://{root}/admin\n')
        server = await start([binary / 'soju', '-config', config], root, 'soju')
        for _ in range(600):
            if (root / 'admin').exists():
                break
            assert server.returncode is None, 'server exited during startup'
            await asyncio.sleep(.1)
        else:
            raise AssertionError('admin listener did not start')
        for user in range(args.users):
            await ctl('user', 'create', '-username', f'u{user}', '-password', 'secret', '-admin')
            await ctl('user', 'run', f'u{user}', 'network', 'create', '-name', 'net', '-addr',
                      f'irc+insecure://127.0.0.1:{base + user % 2}', '-nick', f'u{user}')
        # Rate-limit authentication submissions to stay below the bounded worker queue.
        for first in range(0, args.clients, 20):
            clients.extend(await asyncio.gather(*(connect(i) for i in range(first, min(first + 20, args.clients)))))
        for writer in clients[:args.users]:
            writer.write(b'JOIN #soak\r\n')
        await asyncio.sleep(3)
        begin = time.monotonic()
        sample(0)
        sent, churn = 0, 0
        while time.monotonic() - begin < args.seconds:
            assert all(p.returncode is None for p in processes if p is not processes[0] and p is not processes[2]), 'process exited'
            index = rng.randrange(args.users)
            clients[index].write(f'PRIVMSG #soak :message {sent}\r\nPING :probe{sent}\r\n'.encode())
            clients[index].write(b'CHATHISTORY LATEST #soak * 10\r\n')
            # Join/part a secondary channel and reconnect one secondary device per tick.
            clients[index].write(b'JOIN #churn\r\nPART #churn :soak\r\n')
            if args.clients > args.users:
                at = rng.randrange(args.users, args.clients)
                clients[at].close()
                with contextlib.suppress(ConnectionError):
                    await clients[at].wait_closed()
                clients[at] = await connect(at)
                churn += 1
            sent += 1
            await asyncio.sleep(1)
            elapsed = time.monotonic() - begin
            if elapsed - samples[-1][0] >= 60:
                sample(elapsed)
        sample(time.monotonic() - begin)
        steady = [(t, rss) for t, rss in samples if t >= min(120, args.seconds / 3)]
        slope = 0
        if len(steady) >= 3:
            xs, ys = zip(*steady)
            slope = statistics.linear_regression(xs, ys).slope * 60
        assert slope <= args.max_growth_kib_minute, f'RSS slope {slope:.1f} KiB/min exceeds budget'
        result = dict(seconds=samples[-1][0], users=args.users, clients=args.clients,
                      messages=sent, reconnects=churn, peak_rss_kib=max(r for _, r in samples),
                      steady_rss_kib_minute=slope)
        (root / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        print(json.dumps(result), flush=True)
    finally:
        for writer in clients:
            writer.close()
        for task in list(tasks):
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        for proc in reversed(processes):
            if proc.returncode is None:
                proc.send_signal(signal.SIGTERM)
                try:
                    await asyncio.wait_for(proc.wait(), 60)
                except asyncio.TimeoutError:
                    proc.kill()
                    await proc.wait()
                    raise AssertionError('shutdown timed out')
                assert proc.returncode == 0, (proc.pid, proc.returncode)
        for log in logs:
            log.close()
        output = (root / 'soju.log').read_text()
        assert not any(x in output for x in ('Sanitizer', 'live objects:', 'LEAK SUMMARY:', 'runtime error:')), output[-10000:]


if __name__ == '__main__':
    asyncio.run(main())
