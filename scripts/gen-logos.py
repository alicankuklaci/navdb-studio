#!/usr/bin/env python3
"""Builds the database logo tiles in crates/navdb-app/assets/logos/.

Each tile is a rounded square in the product colour with the glyph from
assets/logos/glyphs/ on top. Simple Icons glyphs (CC0) are drawn on a 24-unit
grid and scaled into the tile; monogram glyphs (`(monogram)` in their title)
are already laid out for the tile.

    scripts/gen-logos.py
"""
import pathlib
import re

ROOT = pathlib.Path(__file__).resolve().parent.parent / "crates/navdb-app/assets/logos"

# name: (tile colour, glyph colour)
TILES = {
    "mysql": ("#00758F", "#FFFFFF"),
    "mariadb": ("#003545", "#FFFFFF"),
    "postgresql": ("#336791", "#FFFFFF"),
    "sqlite": ("#0F80CC", "#FFFFFF"),
    "redis": ("#DC382D", "#FFFFFF"),
    "sqlserver": ("#B8282B", "#FFFFFF"),
    "oracle": ("#C74634", "#FFFFFF"),
    "mongodb": ("#023430", "#00ED64"),
    "duckdb": ("#FFE33A", "#1A1A1A"),
    "iceberg": ("#2A6FBF", "#FFFFFF"),
    "vertica": ("#1B4F9C", "#FFFFFF"),
    "clickhouse": ("#1B1B1B", "#FAFF69"),
    "trino": ("#DD00A1", "#FFFFFF"),
    "presto": ("#4F7FE8", "#FFFFFF"),
    "opensearch": ("#005EB8", "#FFFFFF"),
    "elasticsearch": ("#005571", "#FFFFFF"),
    "apachecassandra": ("#1287B1", "#FFFFFF"),
    "scylladb": ("#1F8FA8", "#FFFFFF"),
    "tidb": ("#DC150B", "#FFFFFF"),
    "singlestore": ("#9A00E6", "#FFFFFF"),
    "oceanbase": ("#006AFF", "#FFFFFF"),
    "starrocks": ("#3E64E6", "#FFFFFF"),
    "doris": ("#1F8AA6", "#FFFFFF"),
    "cockroachlabs": ("#6933FF", "#FFFFFF"),
    "yugabytedb": ("#F2603A", "#FFFFFF"),
    "timescale": ("#FDB515", "#1A1A1A"),
    "amazonredshift": ("#7B3FE4", "#FFFFFF"),
    "greenplum": ("#3AA14A", "#FFFFFF"),
    "valkey": ("#4E5FD9", "#FFFFFF"),
    "keydb": ("#D7354C", "#FFFFFF"),
    "dragonfly": ("#7A4DFF", "#FFFFFF"),
}

# glyph box inside the 24-unit tile
GLYPH = 13.0
# glyphs that read too small or too heavy at the common size
SCALE = {"mysql": 1.45}
MONOGRAM_SCALE = 0.9


def lighter(hex_color: str, t: float) -> str:
    r, g, b = (int(hex_color[i : i + 2], 16) for i in (1, 3, 5))
    return "#%02X%02X%02X" % tuple(round(c + (255 - c) * t) for c in (r, g, b))


def tile(name: str, color: str, fg: str) -> str:
    src = (ROOT / "glyphs" / f"{name}.svg").read_text()
    title = re.search(r"<title>(.*?)</title>", src).group(1).replace(" (monogram)", "")
    body = re.search(r"<svg[^>]*>(.*)</svg>", src, re.S).group(1)
    body = re.sub(r"<title>.*?</title>", "", body)
    if "(monogram)" in src:
        k = MONOGRAM_SCALE
        glyph = f'<g fill="{fg}" transform="translate({12 - 12 * k:.3f} {12 - 12 * k:.3f}) scale({k})">{body}</g>'
    else:
        size = GLYPH * SCALE.get(name, 1.0)
        off = (24 - size) / 2
        glyph = f'<g fill="{fg}" transform="translate({off:.3f} {off:.3f}) scale({size / 24:.4f})">{body}</g>'
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" viewBox="0 0 24 24">'
        f"<title>{title}</title>"
        f'<defs><linearGradient id="g" x1="0" y1="0" x2="0" y2="1">'
        f'<stop offset="0" stop-color="{lighter(color, 0.16)}"/><stop offset="1" stop-color="{color}"/>'
        f"</linearGradient></defs>"
        f'<rect x="1" y="1" width="22" height="22" rx="5.5" fill="url(#g)"/>'
        f'<rect x="1.25" y="1.25" width="21.5" height="21.5" rx="5.25" fill="none" stroke="#FFFFFF" stroke-opacity="0.18" stroke-width="0.5"/>'
        f"{glyph}</svg>\n"
    )


def main() -> None:
    for name, (color, fg) in TILES.items():
        (ROOT / f"{name}.svg").write_text(tile(name, color, fg))
    print(f"wrote {len(TILES)} tiles to {ROOT}")


if __name__ == "__main__":
    main()
