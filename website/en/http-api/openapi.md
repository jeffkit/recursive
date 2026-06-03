# OpenAPI Specification

The server self-documents via an OpenAPI 3.0 specification.

## Fetch the spec

```bash
curl http://localhost:3000/openapi.json | jq .
```

## Use with Swagger UI

You can load the spec into any OpenAPI viewer. With `npx`:

```bash
npx @redocly/cli preview-docs http://localhost:3000/openapi.json
```

Or use the online [Swagger Editor](https://editor.swagger.io) — paste the URL `http://localhost:3000/openapi.json`.
