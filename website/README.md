# groundtruth website

Landing page and documentation built with [Astro](https://astro.build) + [Starlight](https://starlight.astro.build).

## Local development

```sh
npm install
npm run dev
```

Open `http://localhost:4321`.

## Build

```sh
npm run build
# Output in dist/
```

## Deploy on Vercel

1. Import the `jondot/groundtruth` repo into Vercel.
2. Set **Root Directory** to `website`.
3. Framework is auto-detected as **Astro**.
4. Build command: `astro build` (auto-detected).
5. Output directory: `dist` (auto-detected).
6. No custom config needed. Static output (`output: 'static'`) requires no adapter.

Environment variables: none required for the static build.
