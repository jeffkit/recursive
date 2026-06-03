# OpenAPI 规范

服务器通过 OpenAPI 3.0 规范自描述。

## 获取规范

```bash
curl http://localhost:3000/openapi.json | jq .
```

## 使用 Swagger UI 查看

```bash
npx @redocly/cli preview-docs http://localhost:3000/openapi.json
```

或使用在线 [Swagger Editor](https://editor.swagger.io)，粘贴 URL `http://localhost:3000/openapi.json`。
