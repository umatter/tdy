-- The clean dataset we want out of a year of drifting monthly exports.
-- Hand-written, reviewed in git, versioned beside the data.
--
-- What a target may say is exactly what reaches the Arrow schema: a name, a
-- type, a nullability. Note what is absent — there is no date format here,
-- because that is a property of a file: these twelve exports carry two
-- different ones and both must land on this one DATE column.

CREATE TABLE sales (
  month      DATE          NOT NULL,
  region     TEXT          NOT NULL,
  amount_chf DECIMAL(14,2) NOT NULL
)
WITH (
  files      = '2025-*.csv, 2025-*.xlsx',
  date_order = 'dmy'
);
