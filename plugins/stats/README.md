# stats

The app's statistics (`docs/specs/07-REPORTS.md` in CortexManage). Every process collects with
`stats-kit` and hands its rows over once per five-minute window (`stats.record`): its errors and
panics (warnings are never collected), and its served calls and model calls summed per hour. This
plugin alone writes them, to `<home>/stats/<table>/<process>/YYYY-MM-DD.parquet` (the
knowledge app's columns, kept 30 days), and after each window offers the rows just written to each
CortexManage on the mesh until one that manages the app's repository accepts them. The day files
are read again only for rows a round could not deliver; a CortexManage that refuses is not asked
again for an hour.
