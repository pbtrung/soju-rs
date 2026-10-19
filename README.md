# soju-rs

A Rust port of [soju], the user-friendly IRC bouncer. soju connects to
upstream IRC servers on behalf of the user to provide extra functionality:
multiple users and networks, detached channels, chat history playback, and
numerous [IRCv3] extensions. soju-rs aims to behave exactly like upstream
soju and works with the same database, configuration file and clients
(e.g. [gamja]).

See [PLAN.md](PLAN.md) for the porting plan and scope.

## License

AGPLv3, see LICENSE.

Copyright (C) 2020 The soju Contributors

[soju]: https://soju.im
[IRCv3]: https://ircv3.net/
[gamja]: https://codeberg.org/emersion/gamja
