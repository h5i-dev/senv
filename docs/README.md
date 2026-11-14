# docs/ — the senv.h5i.dev site

Static HTML, served by GitHub Pages. No generator.

```
index.html        product page for the OpenEnv Echo server
manual/           serve flags, Python client, limits
_static/          shared h5i site chassis (do not edit here)
CNAME             senv.h5i.dev
llms.txt          the same facts, condensed
sitemap.xml
robots.txt
```

`install.sh` is copied into the published site by `.github/workflows/pages.yaml`.
Do not commit a second copy under `docs/`.

When the server's behavior changes, update `README.md`, `PROTOCOL.md`, and
`DESIGN.md` first, then this site. The pages must not claim a Python sandbox,
a uv workflow, or an OS security boundary. Those belonged to the previous
product in this repository's history.
