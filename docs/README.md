# docs/ — the senv.h5i.dev site

Static HTML, served by GitHub Pages. No generator.

```
index.html        product page for the senv library
manual/           the Environment trait, serve flags, Python clients, limits
_static/          shared h5i site chassis (do not edit here)
CNAME             senv.h5i.dev
llms.txt          the same facts, condensed
sitemap.xml
robots.txt
```

When the server's behavior changes, update `README.md`, `PROTOCOL.md`, and
`DESIGN.md` first, then this site. The pages must not claim a Python sandbox,
a uv workflow, or an OS security boundary. Those belonged to the previous
product in this repository's history.
