#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Generate the fixture for the draft's rounding declaration on a
currency-formatted float column (found by the 2026-09-30 workbook sweep's
bisect on `2021_utility-scale_solar_data_update_0.xlsm`).

Run from the repo root: python3 testdata/gen/18_draft_rounding.py

Deterministic; requires openpyxl (and lxml, transitively -- see
gen_fixtures.py's module docstring).

--------------------------------------------------------------------------
FIXTURES
--------------------------------------------------------------------------

1. draft_float_money.xlsx (`tests/draft.rs::
   a_currency_formatted_float_column_drafts_with_rounding_declared`,
   `the_unedited_draft_fits_a_currency_formatted_float_file`)

   One sheet, header `Site | Energy Value 2020$/MWh | Capacity Value
   2020$/MWh`, 520 data rows, both value columns currency-formatted
   (`"$"#,##0.00`).

   `Energy Value 2020$/MWh` is float noise wearing a currency format, as in
   the solar workbook: the sniffer types a currency-formatted column at the
   widest scale its sample (the first TYPE_SAMPLE = 500 body rows) carries,
   up to MAX_MONEY_SCALE = 15. Row 3 carries 0.012345678901234 (15
   fractional digits), so the sample says DECIMAL(38,15); row 510 -- past
   the sample -- carries 0.0958315743646689 (16 fractional digits, Rust's
   and Python's shortest round-trip of the stored double alike). A target
   declaring DECIMAL(38,15) without `round = 'half_away'` refuses that
   value, correctly; the draft must therefore declare the rounding. Every
   other row is `NN.ddddd` (5 places).

   `Capacity Value 2020$/MWh` is the in-file control: money at 2 places
   (1.00 to 3.00 in quarters), which drafts with no rounding declared.

   Ground truth (computed below and asserted, exact Decimal arithmetic):
   with row 510 rounded half away from zero at scale 15
   (0.0958315743646689 -> 0.095831574364669), the energy column sums to
   ENERGY_SUM_AT_15 = 23357.561107253265903; capacity sums to 1040.00.
"""

import os
import re
import zipfile
from decimal import Decimal, ROUND_HALF_UP

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
OUT = os.path.join(REPO, "testdata")

ROWS = 520
NOISY_ROW = 510  # 1-based data row, past the 500-row type sample
WIDE_ROW = 3  # 1-based data row carrying the sample's widest scale (15)
NOISY = "0.0958315743646689"
WIDE = "0.012345678901234"
ENERGY_SUM_AT_15 = Decimal("23357.561107253265903")
CAPACITY_SUM = Decimal("1040.00")


def note(path, what):
    print(f"wrote {os.path.relpath(path, REPO)} ({os.path.getsize(path)} bytes) - {what}")


_MODIFIED_RE = re.compile(rb"(<dcterms:modified[^>]*>)[^<]*(</dcterms:modified>)")


def repack_deterministic(path):
    """Pinned entry timestamps and dcterms:modified, as every xlsx-writing
    generator here does -- openpyxl rewrites dcterms:modified at save time."""
    tmp = path + ".tmp"
    with zipfile.ZipFile(path) as zin:
        entries = [(i.filename, zin.read(i.filename)) for i in zin.infolist()]
    entries = [
        (name, _MODIFIED_RE.sub(rb"\g<1>2026-01-01T00:00:00Z\g<2>", data)
         if name == "docProps/core.xml" else data)
        for name, data in entries
    ]
    with zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED, compresslevel=6) as zout:
        for name, data in entries:
            info = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.create_system = 0
            info.external_attr = 0o644 << 16
            zout.writestr(info, data)
    os.replace(tmp, path)


def energy(i):
    """The 1-based data row's energy value, as the decimal string the
    stored double reads back as."""
    if i == WIDE_ROW:
        return WIDE
    if i == NOISY_ROW:
        return NOISY
    return f"{20 + (i * 37) % 50}.{(i * 7919) % 100000:05d}"


def capacity(i):
    return str(Decimal(1) + Decimal(i % 9) * Decimal("0.25"))


def build_draft_float_money():
    from datetime import datetime
    from openpyxl import Workbook

    wb = Workbook()
    ws = wb.active
    ws.title = "Sheet1"
    fmt = '"$"#,##0.00'
    ws.append(["Site", "Energy Value 2020$/MWh", "Capacity Value 2020$/MWh"])
    energy_sum = Decimal(0)
    capacity_sum = Decimal(0)
    for i in range(1, ROWS + 1):
        e, c = float(energy(i)), float(capacity(i))
        # The double must read back as exactly the string chosen, or the
        # scale claims above are about some other number.
        assert Decimal(repr(e)) == Decimal(energy(i)), (i, repr(e))
        ws.append([f"Site {i:03d}", e, c])
        energy_sum += Decimal(repr(e))
        capacity_sum += Decimal(repr(c))
    for row in range(2, ROWS + 2):
        for col in (2, 3):
            ws.cell(row=row, column=col).number_format = fmt

    noisy = Decimal(NOISY)
    assert -noisy.as_tuple().exponent == 16
    assert -Decimal(WIDE).as_tuple().exponent == 15
    rounded = noisy.quantize(Decimal(10) ** -15, rounding=ROUND_HALF_UP)
    assert str(rounded) == "0.095831574364669", rounded
    assert energy_sum - noisy + rounded == ENERGY_SUM_AT_15, energy_sum - noisy + rounded
    assert capacity_sum == CAPACITY_SUM, capacity_sum

    wb.properties.created = wb.properties.modified = datetime(2026, 1, 1)
    out = os.path.join(OUT, "draft_float_money.xlsx")
    wb.save(out)
    repack_deterministic(out)
    note(out, "currency-formatted float column, a 16-place value past the type sample")


def main():
    os.makedirs(OUT, exist_ok=True)
    build_draft_float_money()


if __name__ == "__main__":
    main()
