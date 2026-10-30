#!/usr/bin/env python3
"""Rebuild small, synthetic interpretation inputs with only the Python standard library."""
import csv
import io
import json
from pathlib import Path
from zipfile import ZIP_STORED, ZipFile, ZipInfo

ROOT = Path(__file__).resolve().parent / "interpretation-fixture"


def fixture_bytes():
    files = {
        "requirements-v1.md": b"# Delivery requirements\n\nR1: Retain audit logs for 30 days.\nR2: Export reports as CSV.\n",
        "requirements-format.md": b"# Delivery requirements\n\n- R1: Retain audit logs for 30 days.\n\n- R2: Export reports as CSV.\n",
        "requirements-v2.md": b"# Delivery requirements\n\nR1: Retain audit logs for 90 days.\nR2: Export reports as CSV.\n",
        "durations.csv": b"id,duration,unit\nA,900,ms\nB,1.2,s\nC,0.01,min\nD,0.0005,h\n",
        "money.csv": b"id,amount,currency\nM1,0.10,USD\nM2,0.20,USD\nM3,9007199254740993.01,USD\nM4,-0.01,USD\n",
        "source-a.txt": b"Draft planning note, 2026-01-08.\nOwner: Mira.\nThe launch is approved for March 4.\n",
        "source-b.txt": b"Draft planning note, 2026-01-08.\nOwner: Jules.\nThe launch is not approved for March 4.\n",
        "source-register.json": b'{"authority_order": null, "status": "two unratified drafts; no deciding source supplied"}\n',
        "nested.json": b'{"batches":[{"id":"B1","orders":[{"id":"O1","lines":[{"sku":"X","quantity":2},{"sku":"Y","quantity":1}]}]},{"id":"B2","orders":[{"id":"O2","lines":[{"sku":"X","quantity":3}]}]}]}\n',
        "events.jsonl": b'{"event_id":"E1","payload":{"duration":{"value":250,"unit":"ms"}}}\n{"event_id":"E2","payload":{"duration":{"value":2,"unit":"s"}}}\n{"event_id":"E3","payload":{"duration":null}}\n',
        "transcript-double.json": b'{"kind":"synthetic_transcription_double","live_audio_processed":false,"segments":[{"id":"S1","start_seconds":1.0,"end_seconds":2.5,"text":"Ship on [Tuesday / Thursday].","alternatives":["Tuesday","Thursday"]}],"recording_available":false}\n',
        "scan-boundary.json": b'{"kind":"synthetic_scan_boundary_manifest","pages":[{"page":1,"image_only":true,"text_extraction":"","ocr_status":"not_run"}],"scan_available":false,"ocr_output_available":false}\n',
    }
    rows = io.StringIO(newline="")
    writer = csv.writer(rows, lineterminator="\n")
    writer.writerow(["id", "value"])
    writer.writerows((f"R{i:03d}", 10000 if i == 137 else 10) for i in range(1, 151))
    files["measurements.csv"] = rows.getvalue().encode()
    # Minimal real XLSX package. A formula's cached 999 disagrees with A1+A2=3;
    # calcMode=manual and fullCalcOnLoad request recalculation, not verification.
    parts = {
        "[Content_Types].xml": '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>',
        "_rels/.rels": '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>',
        "xl/workbook.xml": '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Totals" sheetId="1" r:id="rId1"/></sheets><calcPr calcMode="manual" fullCalcOnLoad="1"/></workbook>',
        "xl/_rels/workbook.xml.rels": '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>',
        "xl/worksheets/sheet1.xml": '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>1</v></c></row><row r="2"><c r="A2"><v>2</v></c></row><row r="3"><c r="A3"><f>A1+A2</f><v>999</v></c></row></sheetData></worksheet>',
    }
    buffer = io.BytesIO()
    with ZipFile(buffer, "w", compression=ZIP_STORED) as archive:
        for name, text in sorted(parts.items()):
            entry = ZipInfo(name, date_time=(2020, 1, 1, 0, 0, 0))
            entry.external_attr = 0o600 << 16
            archive.writestr(entry, text.encode())
    files["formula-cache.xlsx"] = buffer.getvalue()
    return files


def main():
    ROOT.mkdir(exist_ok=True)
    for name, content in fixture_bytes().items():
        (ROOT / name).write_bytes(content)
    print(f"Generated {len(fixture_bytes())} synthetic inputs in {ROOT}")


if __name__ == "__main__":
    main()
