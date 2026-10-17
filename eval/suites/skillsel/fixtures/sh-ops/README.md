# ops

Scripts for the small box that serves the status page in `www/`.

- `configure.sh` asks for the install settings and writes `settings.env`.
- `install.sh` installs the scripts (the box runs Alpine, `/bin/sh` is busybox ash).
- `backup.sh` copies `www/` and `settings.env` to `backups/<date>/`.
- `gen_report.sh` checks every page and prints a summary at the end.
