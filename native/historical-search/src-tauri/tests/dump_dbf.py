"""Test helper only: dumps every record of a DBF table as JSON using the
already-validated prototype parser (pp_dbf.py), so the Rust port can be
compared against it record-for-record, field-for-field. Every value is
stringified the same way the real app actually consumes these fields
(str(value), "" for empty/None) rather than kept as Python's native
int/float/str, so the comparison reflects real usage, not incidental typing.

Usage: python3 dump_dbf.py <path-to-.dbf> <path-to-prototype-dir>
"""
import sys
import json

sys.path.insert(0, sys.argv[2])
from pp_dbf import DBFTable

table = DBFTable(sys.argv[1])
records = [
    {k: ("" if v is None else str(v)) for k, v in rec.items()}
    for rec in table.records()
]
table.close()
print(json.dumps(records))
