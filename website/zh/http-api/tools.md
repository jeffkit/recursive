# Tools API

## 列出工具

返回所有已注册工具及其定义。

```http
GET /tools
```

**响应**：
```json
[
  {
    "name": "read_file",
    "description": "读取文件内容。",
    "parameters": {
      "type": "object",
      "properties": {
        "path": { "type": "string", "description": "相对于工作区的文件路径" }
      },
      "required": ["path"]
    }
  }
]
```
