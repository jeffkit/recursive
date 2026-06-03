# recursive tools

列出已注册的工具。无需 API Key。

```bash
recursive tools [选项]
```

## 说明

打印当前配置中注册的所有工具，包括名称、描述和 JSON Schema 参数定义。

适合调试工具注册情况，了解模型可以调用哪些工具。

## 选项

| 选项 | 默认值 | 说明 |
|---|---|---|
| `--json` | 关 | 以 JSON 格式输出工具定义 |
| `--workspace <path>` | 当前目录 | 沙箱根目录（影响路径相关工具） |
