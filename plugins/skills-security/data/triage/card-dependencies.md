1. A malicious package (MAL- advisory) or a campaign signal: alert, whatever else holds.
2. Exploited in the wild (KEV) and reachable is called or imported: alert.
3. Reachable is called: alert. Imported and severity high or critical: alert.
4. Reachable is dev and severity is not critical: dismiss; the code never ships it.
5. Reachable is not_imported or transitive, no KEV, EPSS under 1%: hardening; the upgrade is still worth it.
6. Reachable is unknown and severity high or critical: investigate.
7. Low or medium severity, no KEV: hardening.
