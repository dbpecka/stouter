# Stouter website

Static marketing and documentation site for Stouter. No build step: just
`index.html`, `style.css`, and `favicon.svg`.

Preview locally:

```sh
python3 -m http.server -d public-web 8000
```

Deploy by serving this directory from any static host (GitHub Pages,
Netlify, Cloudflare Pages, S3, nginx).
