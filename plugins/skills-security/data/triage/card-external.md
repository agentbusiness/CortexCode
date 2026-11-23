1. A program, connection or file a developer machine runs every day (an editor, a package manager, a signed vendor app, a known dev server on localhost): dismiss.
2. Persistence nobody installed knowingly (a new launch agent, a login item, a shell start-file line) that runs a script from a temporary, download or hidden folder: alert.
3. A process that reads credential files (~/.ssh, ~/.npmrc, the keychain) and opens the network: alert.
4. A listener on all interfaces for a development server: hardening.
5. Unsigned or recently created and the facts do not say what it does: investigate.
