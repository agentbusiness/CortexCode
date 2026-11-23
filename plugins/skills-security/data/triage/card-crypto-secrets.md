1. A broken algorithm (MD5, SHA-1, DES, ECB) protecting passwords, tokens or signatures: alert.
2. The same algorithm used for a cache key, a checksum or a file id: dismiss.
3. Random numbers from a non-cryptographic source for tokens, ids or keys: alert.
4. Certificate checks turned off in code that ships: alert; in a test: dismiss.
