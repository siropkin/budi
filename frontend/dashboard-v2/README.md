# budi dashboard

React + TypeScript dashboard app for `/dashboard`.

## Development

```bash
npm install
npm run dev
```

`vite` runs on `http://127.0.0.1:5174` and proxies daemon API calls to `http://127.0.0.1:7878`.

## Production build

```bash
npm run build
```

Build output is written directly into `crates/budi-daemon/static/dashboard-dist` and served by `budi-daemon` at `/dashboard`.
