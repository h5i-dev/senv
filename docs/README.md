# docs/ — the senv.h5i.dev site

Static HTML, served verbatim by GitHub Pages. No generator, no build step, no
dependencies to install. Edit a file, push to `main`, and
`.github/workflows/pages.yaml` deploys it.

```
index.html        the product page: what the boundary is, the two phases, limits, FAQ
manual/           the reference: every command and flag, senv.toml, tiers, receipts
_static/          the h5i site chassis, shared verbatim with h5i.dev
CNAME             senv.h5i.dev
llms.txt          the same material, condensed for machine readers like LLMs
sitemap.xml       both canonical pages; add a row when you add a page
robots.txt        allow everything, point at the sitemap
.nojekyll         only matters if Pages is ever repointed at a branch
```

## Two things that are not in this directory

**`install.sh`** is copied here from the repository root by the deploy
workflow, so `https://senv.h5i.dev/install.sh` and `./install.sh` can never be
two different scripts. Do not commit a second copy.

**`_static/`** is a verbatim copy of `h5i/docs/_static` (`blog.css`,
`highlight.css`, `blog.js`, `highlight.js`, `logo.png`). senv is a subdomain of
the same product family and shares its chassis rather than forking a second
design system. If h5i's chassis changes in a way senv should follow, re-copy
the files; do not edit them here, because an edit that lives only in this
repository is a fork nobody declared.

## When you change what senv does

The pages state enforcement facts, and a stale security claim is worse than no
page. The sources of truth are `README.md`, `DESIGN.md`, and `src/cli.rs`; when
one of those changes, check:

- the two-phase table on `index.html` and its twin in `manual/`;
- the command and flag tables in `manual/` against `src/cli.rs`;
- the `senv.toml` schema in `manual/` against `src/config.rs`;
- the limits list, which lives in `manual/` and in `llms.txt` (the product page
  keeps only the FAQ's short version, and links out for the rest);
- `dateModified` in the manual's JSON-LD and `lastmod` in `sitemap.xml`.

Every claim on these pages should name the mechanism that backs it. If a
mechanism is gone, the sentence goes with it.
