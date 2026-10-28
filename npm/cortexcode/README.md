# cortexcode

```sh
npx cortexcode
```

Starts [CortexCode](https://github.com/agentbusiness/CortexCode), a local coding agent. The first
run downloads the app for this machine (Apple Silicon Macs; Linux x64 and arm64) from the official
releases, checks it against the checksum in the release catalog and unpacks it into `~/.cortexcode/app/`.
Later runs start the installed app at once; the app updates its frame and plugins itself. The app
runs on its own: you can close the terminal (its start output goes to `~/.cortexcode/logs/start.log`);
running the command again while it runs only says so.

Arguments go to the app: `npx cortexcode --headless`, `npx cortexcode --safe`.
