# Third-party software

Graphite's original code is distributed under GPL-3.0-only; see `LICENSE`.
Copyright © 2026 Graphite contributors. There is no warranty.

The optional `ogdf` feature compiles OGDF from the pinned Bandage submodule.
Bandage includes GPLv3 in `Bandage/COPYING`; the bundled OGDF files specify
GPL version 2 or 3 in `Bandage/ogdf/LICENSE.txt` and their source headers.
Those notices remain intact. The native bridge follows the settings of
Bandage's graph layout worker. See `LAYOUT.md` for algorithmic provenance.
The default build does not compile Bandage or OGDF.

Rust dependencies keep their own licenses. Release packages include a generated
`dependency-licenses.json` inventory and available license/notice files in
`third-party-licenses/`, including the fonts embedded through egui. The matching
source archive contains the locked dependency sources and an offline Cargo
configuration, plus the pinned Bandage source for the optional backend.
Generate these with `scripts/package_release.py`.

GPL permits commercial use and sale; it does not impose a noncommercial
restriction. Redistribution must follow its terms, including the applicable
source-code obligations. See the [GPL FAQ](https://www.gnu.org/licenses/gpl-faq.html).

The example graph is synthetic and part of Graphite. Compatibility datasets
are downloaded separately from their listed upstream sources and retain their
upstream terms; they are not part of the binary release packages.
