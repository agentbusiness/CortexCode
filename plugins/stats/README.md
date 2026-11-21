# stats

The app's statistics (`docs/specs/07-REPORTS.md` in CortexManage). Every process collects with
`stats-kit` and hands its rows over once a minute (`stats.record`). This plugin alone writes them,
to `<home>/stats/<table>/<process>/YYYY-MM-DD.parquet` (the knowledge app's columns, kept 30 days), and once
a minute offers the rows no CortexManage has accepted yet to each CortexManage on the mesh, until
one that manages the app's repository accepts them.
