1. A credential the provider confirmed live: alert.
2. A real key format outside tests and examples: alert; anyone with a copy of the repository has it.
3. The value is a placeholder, a documented example or a test fixture: dismiss.
4. A secret only in git history, not in the tree: alert; history travels with every clone.
5. A debug flag, an open admin route or default credentials in shipped config: alert; in a test config: dismiss.
