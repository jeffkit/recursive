# Tools API

## List tools

Returns all registered tools with their definitions.

```http
GET /tools
```

**Response**:
```json
[
  {
    "name": "read_file",
    "description": "Read the contents of a file.",
    "parameters": {
      "type": "object",
      "properties": {
        "path": { "type": "string", "description": "File path relative to workspace" }
      },
      "required": ["path"]
    }
  },
  ...
]
```
