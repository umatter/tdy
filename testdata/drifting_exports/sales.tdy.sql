-- The clean dataset we want out of a year of drifting monthly exports.
-- Hand-written, reviewed in git, versioned beside the data.
--
-- What a target may say is exactly what reaches the Arrow schema: a name, a
-- type, a nullability. Note what is absent — there is no date format here,
-- because that is a property of a file: these twelve exports carry two
-- different ones and both must land on this one DATE column.
--
-- `matches` is the other half. A target names what we *want*; the files are
-- somebody else's exports and say Datum, Betrag, Amount. Nothing bridges that
-- automatically, and a planner guessing at synonyms is exactly what this tool
-- does not do — so the synonyms are declared here, in the open, in a diff.

CREATE TABLE sales (
  month      DATE          NOT NULL OPTIONS(matches = 'Datum, Date, Buchungsdatum'),
  region     TEXT          NOT NULL OPTIONS(matches = 'Region, Kanton, Gebiet'),
  amount_chf DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag, Betrag CHF, Amount, Umsatz')
)
WITH (
  files      = '2025-*.csv, 2025-*.xlsx',
  date_order = 'dmy'
);
