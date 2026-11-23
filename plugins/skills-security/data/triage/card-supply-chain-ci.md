1. A malicious install hook, a known payload hash, a campaign file or planted persistence: alert. The machine's credentials may already be gone.
2. A workflow that runs a pull request's code with secrets, or caches from untrusted runs feeding a release: alert.
3. Template injection of an issue, comment or branch name into a run step: alert when the workflow triggers on issues, comments or pull_request_target; else investigate.
4. A missing release cooldown, install scripts allowed, an action pinned to a tag: hardening.
5. A dependency fetched from a git commit inside an installed package: investigate.
