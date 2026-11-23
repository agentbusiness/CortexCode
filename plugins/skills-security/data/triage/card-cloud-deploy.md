1. A public bucket, an open security group to the world on a database or admin port, or a wildcard IAM policy: alert.
2. A container that runs as root or privileged in production: hardening; with the Docker socket mounted: alert.
3. A secret written into an image or a manifest: alert.
4. The same in a local or test environment file: dismiss.
