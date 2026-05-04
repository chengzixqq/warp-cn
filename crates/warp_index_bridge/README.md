# warp-index-bridge

Minimal stdio MCP server exposing Warp's local codebase-index metadata to external MCP clients (Claude Code, etc.).

## Why

Counterpart to the in-process `AuggieStoreClient`: Warp uses Auggie to *do* the indexing; this bridge lets external tools *see* what Warp has indexed (path digests, last-modified times) and decide their own retrieval strategy.

## Build

```bash
cargo build -p warp_index_bridge --release
```

Output: `target/release/warp-index-bridge`.

## Register with Claude Code

Add to `~/.claude.json`:

```jsonc
{
  "mcpServers": {
    "warp-index-bridge": {
      "command": "/Users/<you>/warp/target/release/warp-index-bridge",
      "args": []
    }
  }
}
```

## Tools

| Tool | Purpose |
|---|---|
| `list_indexed_snapshots()` | Enumerate every `snapshot_*` file in Warp's `codebase_index_snapshots/` directory with mtime + size + extracted digest. |
| `digest_for_repo_path(repo_path)` | Compute the same `DefaultHasher`-based digest Warp uses, so a client can correlate a known absolute repo path back to a snapshot file. |

## Scope (v1)

- ✅ Read filesystem metadata under `secure_state_dir() / codebase_index_snapshots/`.
- ❌ **Not yet:** parse the bincode snapshot to extract repo paths, fragment counts, or `last_synced_at`. Deferred until upstream Warp commits to a stable on-disk schema. The current digest-based correlation is one-way; clients must already know the absolute repo path to look it up.
- ❌ **Not yet:** read the SQLite `workspace_metadata` table.
- ❌ **Not yet:** detect whether a Warp process is currently running.

## Extension points

If you add more tools later, keep them strictly read-only and Cargo-feature-gate any that would link Warp's full ai crate (which carries GraphQL deps).
