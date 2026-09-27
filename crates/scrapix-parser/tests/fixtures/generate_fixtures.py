#!/usr/bin/env python3
"""Generate the document-parsing test fixtures (stdlib only).

    python3 crates/scrapix-parser/tests/fixtures/generate_fixtures.py

Writes, next to this script:

- report.docx / report.xlsx / report.pptx / report.epub / report.odt —
  minimal but valid office packages with a heading and a small table.
- text-table.pdf — a text-based PDF: a title, a paragraph and a ruled
  3x4 table (drawn with `re` operators, which pdf-inspector detects).
- scanned.pdf — one page that is only a JPEG of text (no text layer).
- mixed.pdf — page 1 text-based, page 2 the scanned image.

The scanned page image (scan.jpg) is rendered from a throwaway PDF with
macOS `sips`; on other systems pass an existing JPEG path as argv[1].
The generated files are committed, so this only needs re-running when a
fixture changes.
"""

import os
import struct
import subprocess
import sys
import tempfile
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))


# --------------------------------------------------------------------------
# PDF
# --------------------------------------------------------------------------


def pdf_bytes(objects):
    """Serialize numbered objects (1..n; object 1 is the catalog)."""
    out = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = []
    for i, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out += f"{i} 0 obj\n".encode()
        out += body if isinstance(body, bytes) else body.encode()
        out += b"\nendobj\n"
    xref = len(out)
    out += f"xref\n0 {len(objects) + 1}\n0000000000 65535 f \n".encode()
    for off in offsets:
        out += f"{off:010d} 00000 n \n".encode()
    out += (
        f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R /Info {len(objects)} 0 R >>\n"
        f"startxref\n{xref}\n%%EOF\n"
    ).encode()
    return bytes(out)


def stream(content, extra=""):
    data = content if isinstance(content, bytes) else content.encode("latin-1")
    return f"<< /Length {len(data)} {extra}>>\nstream\n".encode() + data + b"\nendstream"


def esc(text):
    return text.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")


def text_page_content():
    ops = [
        "BT /F2 20 Tf 72 720 Td (Quarterly Report) Tj ET",
        "BT /F1 11 Tf 72 692 Td (Revenue grew in every region this quarter, led by the West.) Tj ET",
    ]
    rows = [("Region", "Q1", "Q2"), ("North", "120", "135"), ("South", "98", "110"), ("West", "143", "150")]
    x0, y_top, w, h = 72, 660, 150, 22
    ops.append("0.8 w")
    for r, row in enumerate(rows):
        y = y_top - (r + 1) * h
        for c, cell in enumerate(row):
            x = x0 + c * w
            ops.append(f"{x} {y} {w} {h} re S")
            font = "/F2" if r == 0 else "/F1"
            ops.append(f"BT {font} 11 Tf {x + 6} {y + 7} Td ({esc(cell)}) Tj ET")
    ops.append(
        "BT /F1 11 Tf 72 540 Td (See https://example.com/methodology for the method.) Tj ET"
    )
    return "\n".join(ops)


FONTS = (
    "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
    "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>",
)


def jpeg_size(data):
    i = 2
    while i < len(data):
        if data[i] != 0xFF:
            i += 1
            continue
        marker = data[i + 1]
        length = struct.unpack(">H", data[i + 2 : i + 4])[0]
        if marker in (0xC0, 0xC1, 0xC2):
            h, w = struct.unpack(">HH", data[i + 5 : i + 9])
            return w, h
        i += 2 + length
    raise ValueError("no SOF marker")


def build_pdf(pages, title):
    """pages: list of ("text", None) or ("image", jpeg_bytes)."""
    # Layout: 1 catalog, 2 pages, 3 F1, 4 F2, then per page: page, content, [image]; last: info.
    objects = [None, None, FONTS[0], FONTS[1]]
    kids = []
    for kind, payload in pages:
        page_num = len(objects) + 1
        kids.append(f"{page_num} 0 R")
        if kind == "text":
            objects.append(
                f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
                f"/Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {page_num + 1} 0 R >>"
            )
            objects.append(stream(text_page_content()))
        else:
            w, h = jpeg_size(payload)
            objects.append(
                f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
                f"/Resources << /XObject << /Im1 {page_num + 2} 0 R >> >> /Contents {page_num + 1} 0 R >>"
            )
            objects.append(stream("q 612 0 0 792 0 0 cm /Im1 Do Q"))
            objects.append(
                stream(
                    payload,
                    f"/Type /XObject /Subtype /Image /Width {w} /Height {h} "
                    f"/ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode ",
                )
            )
    objects[0] = "<< /Type /Catalog /Pages 2 0 R >>"
    objects[1] = f"<< /Type /Pages /Kids [{' '.join(kids)}] /Count {len(kids)} >>"
    objects.append(f"<< /Title ({esc(title)}) /Producer (scrapix fixtures) >>")
    return pdf_bytes(objects)


def scan_source_pdf():
    lines = ["SCANNED INVOICE", "Invoice number 4821", "Customer ACME Corp", "Total due 1250 EUR"]
    ops = []
    for i, line in enumerate(lines):
        size = 34 if i == 0 else 26
        ops.append(f"BT /F2 {size} Tf 72 {700 - i * 60} Td ({esc(line)}) Tj ET")
    return pdf_bytes(
        [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
            "/Resources << /Font << /F2 5 0 R >> >> /Contents 4 0 R >>",
            stream("\n".join(ops)),
            FONTS[1],
            "<< /Producer (scrapix fixtures) >>",
        ]
    )


def scan_jpeg():
    if len(sys.argv) > 1:
        with open(sys.argv[1], "rb") as f:
            return f.read()
    with tempfile.TemporaryDirectory() as tmp:
        src = os.path.join(tmp, "scan-source.pdf")
        png = os.path.join(tmp, "scan.png")
        big = os.path.join(tmp, "scan-big.png")
        jpg = os.path.join(tmp, "scan.jpg")
        with open(src, "wb") as f:
            f.write(scan_source_pdf())
        run = lambda *a: subprocess.run(a, check=True, capture_output=True)
        run("sips", "-s", "format", "png", src, "--out", png)
        # 612x792 at 72 dpi → 2x for a crisper "scan".
        run("sips", "-z", "1584", "1224", png, "--out", big)
        # Flatten onto white (the PDF render has a transparent background).
        run("sips", "-s", "format", "jpeg", "-s", "formatOptions", "85", big, "--out", jpg)
        with open(jpg, "rb") as f:
            return f.read()


# --------------------------------------------------------------------------
# Office packages
# --------------------------------------------------------------------------


def write_zip(name, entries, stored_first=None):
    path = os.path.join(HERE, name)
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        if stored_first:
            z.writestr(zipfile.ZipInfo(stored_first[0]), stored_first[1], zipfile.ZIP_STORED)
        for n, data in entries:
            info = zipfile.ZipInfo(n, date_time=(2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            z.writestr(info, data)
    print("wrote", name)


RELS_NS = "http://schemas.openxmlformats.org/package/2006/relationships"
OFFICE_DOC = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"
W = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"


def docx():
    def para(text, style=None):
        ppr = f'<w:pPr><w:pStyle w:val="{style}"/></w:pPr>' if style else ""
        return f"<w:p>{ppr}<w:r><w:t xml:space=\"preserve\">{text}</w:t></w:r></w:p>"

    def cell(text):
        return f"<w:tc><w:tcPr><w:tcW w:w=\"2000\" w:type=\"dxa\"/></w:tcPr>{para(text)}</w:tc>"

    rows = [("Region", "Q1", "Q2"), ("North", "120", "135"), ("West", "143", "150")]
    table = "<w:tbl><w:tblPr><w:tblW w:w=\"6000\" w:type=\"dxa\"/></w:tblPr><w:tblGrid>" + (
        "<w:gridCol w:w=\"2000\"/>" * 3
    ) + "</w:tblGrid>" + "".join(
        "<w:tr>" + "".join(cell(c) for c in row) + "</w:tr>" for row in rows
    ) + "</w:tbl>"
    document = (
        f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        f'<w:document xmlns:w="{W}"><w:body>'
        + para("Quarterly Report", "Heading1")
        + para("Revenue grew in every region this quarter, led by the West.")
        + table
        + "<w:sectPr/></w:body></w:document>"
    )
    styles = (
        f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:styles xmlns:w="{W}">'
        '<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style>'
        '<w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/>'
        '<w:basedOn w:val="Normal"/><w:pPr><w:outlineLvl w:val="0"/></w:pPr></w:style></w:styles>'
    )
    write_zip(
        "report.docx",
        [
            (
                "[Content_Types].xml",
                '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
                '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
                '<Default Extension="xml" ContentType="application/xml"/>'
                '<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>'
                '<Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>'
                "</Types>",
            ),
            (
                "_rels/.rels",
                f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="{RELS_NS}">'
                f'<Relationship Id="rId1" Type="{OFFICE_DOC}" Target="word/document.xml"/></Relationships>',
            ),
            (
                "word/_rels/document.xml.rels",
                f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="{RELS_NS}">'
                '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>'
                "</Relationships>",
            ),
            ("word/document.xml", document),
            ("word/styles.xml", styles),
        ],
    )


def xlsx():
    strings = ["Region", "Q1", "Q2", "North", "West"]
    sst = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        f'<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="{len(strings)}" uniqueCount="{len(strings)}">'
        + "".join(f"<si><t>{s}</t></si>" for s in strings)
        + "</sst>"
    )

    def s(ref, idx):
        return f'<c r="{ref}" t="s"><v>{idx}</v></c>'

    def n(ref, val):
        return f'<c r="{ref}"><v>{val}</v></c>'

    sheet = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>'
        f'<row r="1">{s("A1", 0)}{s("B1", 1)}{s("C1", 2)}</row>'
        f'<row r="2">{s("A2", 3)}{n("B2", 120)}{n("C2", 135)}</row>'
        f'<row r="3">{s("A3", 4)}{n("B3", 143)}{n("C3", 150)}</row>'
        "</sheetData></worksheet>"
    )
    write_zip(
        "report.xlsx",
        [
            (
                "[Content_Types].xml",
                '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
                '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
                '<Default Extension="xml" ContentType="application/xml"/>'
                '<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>'
                '<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>'
                '<Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>'
                "</Types>",
            ),
            (
                "_rels/.rels",
                f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="{RELS_NS}">'
                f'<Relationship Id="rId1" Type="{OFFICE_DOC}" Target="xl/workbook.xml"/></Relationships>',
            ),
            (
                "xl/workbook.xml",
                '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" '
                'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">'
                '<sheets><sheet name="Revenue" sheetId="1" r:id="rId1"/></sheets></workbook>',
            ),
            (
                "xl/_rels/workbook.xml.rels",
                f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="{RELS_NS}">'
                '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>'
                '<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>'
                "</Relationships>",
            ),
            ("xl/worksheets/sheet1.xml", sheet),
            ("xl/sharedStrings.xml", sst),
        ],
    )


def pptx():
    P = "http://schemas.openxmlformats.org/presentationml/2006/main"
    A = "http://schemas.openxmlformats.org/drawingml/2006/main"
    R = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"

    def shape(sid, ph, text):
        return (
            f'<p:sp><p:nvSpPr><p:cNvPr id="{sid}" name="s{sid}"/><p:cNvSpPr/>'
            f'<p:nvPr><p:ph type="{ph}"/></p:nvPr></p:nvSpPr><p:spPr/>'
            f'<p:txBody><a:bodyPr/><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp>'
        )

    slide = (
        f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        f'<p:sld xmlns:p="{P}" xmlns:a="{A}" xmlns:r="{R}"><p:cSld><p:spTree>'
        '<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>'
        + shape(2, "title", "Quarterly Report")
        + shape(3, "body", "Revenue grew in every region this quarter, led by the West.")
        + "</p:spTree></p:cSld></p:sld>"
    )
    write_zip(
        "report.pptx",
        [
            (
                "[Content_Types].xml",
                '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
                '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
                '<Default Extension="xml" ContentType="application/xml"/>'
                '<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>'
                '<Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>'
                "</Types>",
            ),
            (
                "_rels/.rels",
                f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="{RELS_NS}">'
                f'<Relationship Id="rId1" Type="{OFFICE_DOC}" Target="ppt/presentation.xml"/></Relationships>',
            ),
            (
                "ppt/presentation.xml",
                f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                f'<p:presentation xmlns:p="{P}" xmlns:a="{A}" xmlns:r="{R}">'
                '<p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst>'
                '<p:sldSz cx="9144000" cy="6858000"/><p:notesSz cx="6858000" cy="9144000"/></p:presentation>',
            ),
            (
                "ppt/_rels/presentation.xml.rels",
                f'<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="{RELS_NS}">'
                '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/>'
                "</Relationships>",
            ),
            ("ppt/slides/slide1.xml", slide),
        ],
    )


def epub():
    xhtml = (
        '<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE html>'
        '<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Chapter 1</title></head><body>'
        "<h1>Quarterly Report</h1><p>Revenue grew in every region this quarter, led by the West.</p>"
        "<table><tr><th>Region</th><th>Q1</th><th>Q2</th></tr>"
        "<tr><td>North</td><td>120</td><td>135</td></tr><tr><td>West</td><td>143</td><td>150</td></tr></table>"
        "</body></html>"
    )
    opf = (
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">'
        '<metadata xmlns:dc="http://purl.org/dc/elements/1.1/">'
        '<dc:identifier id="uid">urn:uuid:scrapix-fixture</dc:identifier>'
        "<dc:title>Quarterly Report</dc:title><dc:language>en</dc:language></metadata>"
        '<manifest><item id="c1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>'
        '<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/></manifest>'
        '<spine><itemref idref="c1"/></spine></package>'
    )
    nav = (
        '<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE html>'
        '<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">'
        '<head><title>Nav</title></head><body><nav epub:type="toc"><ol>'
        '<li><a href="chapter1.xhtml">Chapter 1</a></li></ol></nav></body></html>'
    )
    write_zip(
        "report.epub",
        [
            (
                "META-INF/container.xml",
                '<?xml version="1.0" encoding="UTF-8"?>'
                '<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">'
                '<rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>'
                "</rootfiles></container>",
            ),
            ("OEBPS/content.opf", opf),
            ("OEBPS/chapter1.xhtml", xhtml),
            ("OEBPS/nav.xhtml", nav),
        ],
        stored_first=("mimetype", "application/epub+zip"),
    )


def odt():
    content = (
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" '
        'xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.2">'
        "<office:body><office:text>"
        '<text:h text:outline-level="1">Quarterly Report</text:h>'
        "<text:p>Revenue grew in every region this quarter, led by the West.</text:p>"
        "</office:text></office:body></office:document-content>"
    )
    manifest = (
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2">'
        '<manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.text"/>'
        '<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>'
        "</manifest:manifest>"
    )
    write_zip(
        "report.odt",
        [("content.xml", content), ("META-INF/manifest.xml", manifest)],
        stored_first=("mimetype", "application/vnd.oasis.opendocument.text"),
    )


def main():
    docx()
    xlsx()
    pptx()
    epub()
    odt()
    jpeg = scan_jpeg()
    for name, pages, title in [
        ("text-table.pdf", [("text", None)], "Quarterly Report"),
        ("scanned.pdf", [("image", jpeg)], "Scanned Invoice"),
        ("mixed.pdf", [("text", None), ("image", jpeg)], "Mixed Report"),
    ]:
        with open(os.path.join(HERE, name), "wb") as f:
            f.write(build_pdf(pages, title))
        print("wrote", name)
    with open(os.path.join(HERE, "scan.jpg"), "wb") as f:
        f.write(jpeg)
    print("wrote scan.jpg")


if __name__ == "__main__":
    main()
