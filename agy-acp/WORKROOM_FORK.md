# Workroom fork notes: agy MCP plugins

Checked against `/usr/local/bin/agy` with `agy --help`, `agy mcp add --help`, `agy plugin validate`, and `agy plugin list`. No login, no network, and no `agy` conversation. `plugin validate` / `plugin list` were pointed at a temporary `HOME` or an explicit directory.

## Where plugins live

Built-in docs, from `strings /usr/local/bin/agy`:

- Global MCP file: `~/.gemini/config/mcp_config.json` (all sessions).
- Plugin MCP file: `plugins/<plugin_name>/mcp_config.json` (active when the plugin is enabled).
- Global customization root: `~/.gemini/config/`.
- Create a plugin at `~/.gemini/config/plugins/<name>/`.

`HOME=/tmp/... agy plugin list --json` lists a plugin placed at `$HOME/.gemini/config/plugins/<name>/` and reports `"enabled": true` with no `config.json` entry. A directory that contains only `mcp_config.json` is not listed.

Workspace plugins (`.agents/plugins/` and the other project roots in the same doc) are separate. This adapter writes only under the process `$HOME`.

## Manifest

`plugin.json` is required. Exact validate error when it is missing:

```text
no plugin.json found at the plugin root or in .antigravity-plugin or .gemini-plugin
```

The docs call it the marker that declares the directory a plugin. Minimum shape:

```json
{ "name": "team-developer-kit" }
```

`name` is optional and defaults to the directory name. Validate warns that `plugin install` refuses a manifest without `name`. This adapter writes `name` equal to the directory name.

`mcp_config.json` is optional in the docs. It is the file that carries the servers. A dotfile next to those two files does not change validate or `plugin list`.

## `mcp_config.json`

Top-level shape from the built-in doc:

```json
{
  "mcpServers": {
    "sqlite-helper": {
      "command": "sqlite-mcp-server",
      "args": ["/path/to/database.db"],
      "env": { "DB_READONLY": "true" }
    },
    "remote-service": {
      "url": "https://mcp.mycompany.com/mcp"
    }
  }
}
```

Stdio fields: `command` (required), `args` (optional string array), `env` (optional object).

Streamable HTTP fields: `url` (required). The legacy key `serverUrl` is also accepted. `agy plugin validate` counts a server that uses either key.

`agy mcp add --help` says `--type` is `stdio` or `http` (default `stdio`) and accepts repeatable `--env KEY=value` and `--header Key: Value`.

`headers` is not in the markdown example. Validate counts a server whose `headers` value is an object and drops a server whose `headers` value is an array of `{name, value}` (`components.mcpServers` goes from 1 to 0, with no finding). This adapter writes an object, same as `env`.

The same doc says the legacy HTTP+SSE transport (MCP spec 2024-11-05) is not supported, and tells you to point `url` at the Streamable HTTP endpoint or to bridge SSE through a stdio process such as `mcp-remote`. Validate still counts a `type: sse` object and an empty object, so that count is not evidence the transport works.

JSONC comments and trailing commas are accepted. This adapter writes plain JSON.

A changelog string in the binary says: "namespacing plugin MCP servers as `<plugin>_<server>`". That namespacing was not observed in a live session. The same docs also say MCP server names are global across the user's config and every installed plugin, and that when two plugins claim the same manifest name the first discovered wins and the other is dropped silently.

## What this adapter does

One directory per ACP session: `$HOME/.gemini/config/plugins/acp-session-<sanitized session id>/` with `plugin.json`, `mcp_config.json`, and an ownership marker. `session/new` and `session/load` rewrite that session's plugin. The process removes it on `session/close`, session eviction, and exit. The next start removes unlocked plugins this adapter marked, and does not touch any other directory.
