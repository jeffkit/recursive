# recursive tools

List registered tools. No API key needed.

```bash
recursive tools [OPTIONS]
```

## Description

Prints all tools registered in the current configuration, including their names, descriptions, and JSON Schema parameter definitions.

Useful for debugging tool registration and understanding what the model can call.

## Example output

```
read_file       Read the contents of a file.
write_file      Write content to a file, creating it if it doesn't exist.
apply_patch     Apply a patch to a file using V4A patch format.
list_dir        List the contents of a directory.
run_shell       Execute a shell command.
search_files    Search for a pattern across files.
```

## Options

| Option | Default | Description |
|---|---|---|
| `--json` | off | Output tool definitions as JSON |
| `--workspace <path>` | cwd | Sandbox root (affects path-based tools) |
