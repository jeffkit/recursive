# recursive sessions

管理持久化会话。

```bash
recursive sessions <子命令>
```

## 子命令

| 子命令 | 说明 |
|---|---|
| `list` | 列出所有已保存的会话 |
| `show <id>` | 显示会话详情 |
| `delete <id>` | 删除会话 |
| `rewind <id> --to-turn <n>` | 将会话回滚到指定轮次 |

## 示例

```bash
# 列出会话
recursive sessions list

# 显示会话
recursive sessions show abc123

# 回滚到第 5 轮
recursive sessions rewind abc123 --to-turn 5

# 删除会话
recursive sessions delete abc123
```

## 会话存储

默认情况下，会话以 JSONL 文件形式存储在 `~/.recursive/sessions/`。

开启 `cloud-runtime` feature 并配置 Redis 后，会话存储在 Redis 中并同步到 S3。
