# importer

Imports the bookings export (`data/bookings.csv`) into the reporting database.

The exporter writes dates day-first (`DD/MM/YYYY`). Every row we recieve is validated before it is
stored; rows that fail are written to `rejected.csv`.

Run the tests with `make test`.
