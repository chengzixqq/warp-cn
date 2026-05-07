//! Backend-only `StoreClient` impl that routes codebase retrieval through the
//! user's local Auggie MCP server. Lives in `app/` because it depends on
//! `AuggieMcpClient` from `app::ai::auggie_mcp`.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use ::ai::index::full_source_code_embedding::{
    store_client::{IntermediateNode, StoreClient},
    CodebaseContextConfig, ContentHash, EmbeddingConfig, Error, Fragment, NodeHash, RepoMetadata,
};
use async_trait::async_trait;
use dashmap::{DashMap, DashSet};
use string_offset::ByteOffset;

use crate::ai::auggie_mcp::AuggieMcpClient;

pub(crate) struct AuggieStoreClient {
    mcp_client: Arc<dyn AuggieMcpClient>,
    repos: DashMap<PathBuf, RepoFragmentCache>,
    synced_nodes: DashSet<NodeHash>,
}

#[derive(Clone)]
struct RepoFragmentCache {
    latest_root: NodeHash,
    fragments_by_hash: HashMap<ContentHash, CachedFragment>,
    fragments_by_path: HashMap<PathBuf, Vec<ContentHash>>,
}

#[derive(Clone)]
struct CachedFragment {
    content_hash: ContentHash,
    absolute_path: PathBuf,
    byte_range: Range<ByteOffset>,
}

impl AuggieStoreClient {
    pub(crate) fn new(mcp_client: Arc<dyn AuggieMcpClient>) -> Self {
        Self {
            mcp_client,
            repos: DashMap::new(),
            synced_nodes: DashSet::new(),
        }
    }
}

#[async_trait]
impl StoreClient for AuggieStoreClient {
    async fn update_intermediate_nodes(
        &self,
        _embedding_config: EmbeddingConfig,
        nodes: Vec<IntermediateNode>,
    ) -> Result<HashMap<NodeHash, bool>, Error> {
        Ok(nodes
            .into_iter()
            .map(|node| {
                self.synced_nodes.insert(node.hash.clone());
                (node.hash, true)
            })
            .collect())
    }

    async fn generate_embeddings(
        &self,
        _embedding_config: EmbeddingConfig,
        fragments: Vec<Fragment>,
        root_hash: NodeHash,
        repo_metadata: RepoMetadata,
    ) -> Result<HashMap<ContentHash, bool>, Error> {
        let statuses = fragments
            .iter()
            .map(|fragment| (fragment.content_hash().clone(), true))
            .collect();

        let Some(repo_path) = canonical_repo_path(&repo_metadata) else {
            log::warn!("auggie store skipped fragment cache update: repo path missing");
            return Ok(statuses);
        };

        let mut cache = self
            .repos
            .entry(repo_path)
            .or_insert_with(|| RepoFragmentCache {
                latest_root: root_hash.clone(),
                fragments_by_hash: HashMap::new(),
                fragments_by_path: HashMap::new(),
            });

        if cache.latest_root != root_hash {
            cache.fragments_by_hash.clear();
            cache.fragments_by_path.clear();
            cache.latest_root = root_hash.clone();
        }

        for fragment in fragments {
            let content_hash = fragment.content_hash().clone();
            let location = fragment.location();
            let absolute_path = dunce::canonicalize(location.absolute_path())
                .unwrap_or_else(|_| location.absolute_path().to_path_buf());
            let cached = CachedFragment {
                content_hash: content_hash.clone(),
                absolute_path: absolute_path.clone(),
                byte_range: location.byte_range(),
            };

            cache
                .fragments_by_path
                .entry(absolute_path)
                .or_default()
                .push(content_hash.clone());
            cache.fragments_by_hash.insert(content_hash, cached);
        }

        Ok(statuses)
    }

    async fn populate_merkle_tree_cache(
        &self,
        _embedding_config: EmbeddingConfig,
        _root_hash: NodeHash,
        _repo_metadata: RepoMetadata,
    ) -> Result<bool, Error> {
        Ok(true)
    }

    async fn sync_merkle_tree(
        &self,
        nodes: Vec<NodeHash>,
        _embedding_config: EmbeddingConfig,
    ) -> Result<HashSet<NodeHash>, Error> {
        Ok(nodes
            .into_iter()
            .filter_map(|node| self.synced_nodes.insert(node.clone()).then_some(node))
            .collect())
    }

    async fn rerank_fragments(
        &self,
        _query: String,
        fragments: Vec<Fragment>,
    ) -> Result<Vec<Fragment>, Error> {
        Ok(fragments)
    }

    async fn get_relevant_fragments(
        &self,
        _embedding_config: EmbeddingConfig,
        query: String,
        root_hash: NodeHash,
        repo_metadata: RepoMetadata,
    ) -> Result<Vec<ContentHash>, Error> {
        let Some(repo_path) = canonical_repo_path(&repo_metadata) else {
            log::warn!("auggie retrieval skipped: repo path missing");
            return Ok(vec![]);
        };

        let Some(cache) = self.repos.get(&repo_path).map(|entry| entry.clone()) else {
            log::warn!(
                "auggie retrieval skipped: fragment cache missing for {}",
                repo_path.display()
            );
            return Ok(vec![]);
        };

        if cache.latest_root != root_hash {
            log::warn!(
                "auggie retrieval skipped: cached root hash stale for {}",
                repo_path.display()
            );
            return Ok(vec![]);
        }

        let excerpts = match self
            .mcp_client
            .codebase_retrieval(query, repo_path.clone())
            .await
        {
            Ok(excerpts) => excerpts,
            Err(err) => {
                log::warn!(
                    "auggie codebase retrieval failed for {}: {err}",
                    repo_path.display()
                );
                return Ok(vec![]);
            }
        };

        let mut matched = Vec::new();
        let mut seen = HashSet::new();

        for excerpt in excerpts {
            let Some(absolute_path) = resolve_excerpt_path(&repo_path, &excerpt.path) else {
                continue;
            };

            let excerpt_range = match line_range_to_byte_range(
                &absolute_path,
                excerpt.line_start,
                excerpt.line_end,
            )
            .await
            {
                Ok(range) => range,
                Err(err) => {
                    log::debug!(
                        "failed to map auggie excerpt lines to bytes for {}: {err}",
                        absolute_path.display()
                    );
                    continue;
                }
            };

            let Some(content_hash) =
                match_excerpt_to_fragment(&cache, &absolute_path, &excerpt_range, &excerpt.content)
                    .await
            else {
                continue;
            };

            if seen.insert(content_hash.clone()) {
                matched.push(content_hash);
            }
        }

        Ok(matched)
    }

    async fn codebase_context_config(&self) -> Result<CodebaseContextConfig, Error> {
        Ok(CodebaseContextConfig {
            embedding_config: EmbeddingConfig::default(),
            embedding_cadence: Duration::from_secs(300),
        })
    }
}

fn canonical_repo_path(repo_metadata: &RepoMetadata) -> Option<PathBuf> {
    let path = repo_metadata.path.as_deref()?;
    match dunce::canonicalize(path) {
        Ok(path) => Some(path),
        Err(err) => {
            log::warn!("failed to canonicalize repo path {path:?}: {err}");
            None
        }
    }
}

fn resolve_excerpt_path(repo_path: &Path, excerpt_path: &Path) -> Option<PathBuf> {
    let candidate = if excerpt_path.is_absolute() {
        excerpt_path.to_path_buf()
    } else {
        repo_path.join(excerpt_path)
    };

    let Ok(canonical) = dunce::canonicalize(&candidate) else {
        log::debug!(
            "dropping auggie excerpt with unresolved path {}",
            candidate.display()
        );
        return None;
    };

    if !canonical.starts_with(repo_path) {
        log::warn!(
            "dropping auggie excerpt outside repo: {} is outside {}",
            canonical.display(),
            repo_path.display()
        );
        return None;
    }

    Some(canonical)
}

async fn match_excerpt_to_fragment(
    cache: &RepoFragmentCache,
    absolute_path: &Path,
    excerpt_range: &Range<ByteOffset>,
    excerpt_content: &str,
) -> Option<ContentHash> {
    let candidates: Vec<CachedFragment> = cache
        .fragments_by_path
        .get(absolute_path)?
        .iter()
        .filter_map(|content_hash| cache.fragments_by_hash.get(content_hash))
        .cloned()
        .collect();

    // Tier 1: byte_range full containment.
    if let Some(fragment) = candidates
        .iter()
        .find(|fragment| contains_range(&fragment.byte_range, excerpt_range))
    {
        return Some(fragment.content_hash.clone());
    }

    // Tier 2: maximum overlap (>0).
    if let Some(fragment) = candidates
        .iter()
        .max_by_key(|fragment| byte_overlap(&fragment.byte_range, excerpt_range))
        .filter(|fragment| byte_overlap(&fragment.byte_range, excerpt_range) > 0)
    {
        return Some(fragment.content_hash.clone());
    }

    // Tier 3: content match (CRLF-normalized).
    let normalized_excerpt = normalize_crlf(excerpt_content);
    for fragment in candidates {
        let Some(content) = read_cached_fragment_content(&fragment).await else {
            continue;
        };
        if normalize_crlf(&content).contains(&normalized_excerpt) {
            return Some(fragment.content_hash);
        }
    }

    None
}

fn contains_range(container: &Range<ByteOffset>, range: &Range<ByteOffset>) -> bool {
    container.start.as_usize() <= range.start.as_usize()
        && container.end.as_usize() >= range.end.as_usize()
}

fn byte_overlap(a: &Range<ByteOffset>, b: &Range<ByteOffset>) -> usize {
    let start = a.start.as_usize().max(b.start.as_usize());
    let end = a.end.as_usize().min(b.end.as_usize());
    end.saturating_sub(start)
}

async fn read_cached_fragment_content(fragment: &CachedFragment) -> Option<String> {
    let content = async_fs::read_to_string(&fragment.absolute_path)
        .await
        .ok()?;
    let start = fragment.byte_range.start.as_usize();
    let end = fragment.byte_range.end.as_usize();

    if start <= end
        && end <= content.len()
        && content.is_char_boundary(start)
        && content.is_char_boundary(end)
    {
        Some(content[start..end].to_string())
    } else {
        log::debug!(
            "cached fragment has invalid byte range {:?} for {}",
            fragment.byte_range,
            fragment.absolute_path.display()
        );
        None
    }
}

fn normalize_crlf(content: &str) -> String {
    content.replace("\r\n", "\n")
}

async fn line_range_to_byte_range(
    path: &Path,
    line_start: usize,
    line_end_inclusive: usize,
) -> Result<Range<ByteOffset>, std::io::Error> {
    let content = async_fs::read_to_string(path).await?;
    let file_len = content.len();
    let start_line = line_start.max(1);
    let end_line = line_end_inclusive.max(start_line);

    let mut line_starts = vec![0usize];
    for (idx, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            line_starts.push(idx + 1);
        }
    }

    let start = line_starts
        .get(start_line.saturating_sub(1))
        .copied()
        .unwrap_or(file_len);
    let end = line_starts.get(end_line).copied().unwrap_or(file_len);

    Ok(ByteOffset::from(start)..ByteOffset::from(end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::auggie_mcp::{AuggieExcerpt, MockAuggieMcpClient};

    fn byte_range(start: usize, end: usize) -> Range<ByteOffset> {
        ByteOffset::from(start)..ByteOffset::from(end)
    }

    fn node_hash(content: &str) -> NodeHash {
        ContentHash::from_content(content).into()
    }

    fn repo_metadata(path: &Path) -> RepoMetadata {
        RepoMetadata {
            path: Some(path.to_string_lossy().to_string()),
        }
    }

    fn test_fragment(content: &str, path: &Path, range: Range<ByteOffset>) -> Fragment {
        let content_hash = ContentHash::from_content(content);
        Fragment::try_from(warp_graphql::queries::rerank_fragments::RerankFragment {
            content: content.to_string(),
            content_hash: warp_graphql::full_source_code_embedding::ContentHash(
                content_hash.to_string(),
            ),
            location: warp_graphql::queries::rerank_fragments::FragmentLocation {
                byte_start: range.start.as_usize() as i32,
                byte_end: range.end.as_usize() as i32,
                file_path: path.to_string_lossy().to_string(),
            },
        })
        .expect("test fragment is valid")
    }

    #[tokio::test]
    async fn get_relevant_fragments_returns_empty_without_cache() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = dunce::canonicalize(temp_dir.path()).unwrap();
        let store = AuggieStoreClient::new(Arc::new(MockAuggieMcpClient::new(vec![])));

        let result = store
            .get_relevant_fragments(
                EmbeddingConfig::default(),
                "query".to_string(),
                node_hash("root"),
                repo_metadata(&repo_path),
            )
            .await
            .unwrap();

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn get_relevant_fragments_matches_by_byte_range_containment() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = dunce::canonicalize(temp_dir.path()).unwrap();
        let file_path = repo_path.join("main.rs");
        let content = "alpha\nbeta\ngamma\n";
        std::fs::write(&file_path, content).unwrap();
        let file_path = dunce::canonicalize(&file_path).unwrap();
        let content_hash = ContentHash::from_content(content);
        let root_hash = node_hash("root");
        let store =
            AuggieStoreClient::new(Arc::new(MockAuggieMcpClient::new(vec![AuggieExcerpt {
                path: PathBuf::from("main.rs"),
                line_start: 2,
                line_end: 2,
                content: "beta".to_string(),
            }])));

        store
            .generate_embeddings(
                EmbeddingConfig::default(),
                vec![test_fragment(
                    content,
                    &file_path,
                    byte_range(0, content.len()),
                )],
                root_hash.clone(),
                repo_metadata(&repo_path),
            )
            .await
            .unwrap();

        let result = store
            .get_relevant_fragments(
                EmbeddingConfig::default(),
                "query".to_string(),
                root_hash,
                repo_metadata(&repo_path),
            )
            .await
            .unwrap();

        assert_eq!(result, vec![content_hash]);
    }

    #[tokio::test]
    async fn line_range_to_byte_range_handles_utf8_crlf_and_out_of_range() {
        let temp_dir = tempfile::tempdir().unwrap();

        let ascii_path = temp_dir.path().join("ascii.txt");
        std::fs::write(&ascii_path, "one\ntwo\nthree").unwrap();
        assert_eq!(
            line_range_to_byte_range(&ascii_path, 2, 2).await.unwrap(),
            byte_range(4, 8)
        );

        let utf8_path = temp_dir.path().join("utf8.txt");
        std::fs::write(&utf8_path, "é\n中\nx").unwrap();
        assert_eq!(
            line_range_to_byte_range(&utf8_path, 2, 2).await.unwrap(),
            byte_range(3, 7)
        );

        let crlf_path = temp_dir.path().join("crlf.txt");
        std::fs::write(&crlf_path, "a\r\nb\r\n").unwrap();
        assert_eq!(
            line_range_to_byte_range(&crlf_path, 1, 1).await.unwrap(),
            byte_range(0, 3)
        );

        // Out-of-range clamps to EOF.
        assert_eq!(
            line_range_to_byte_range(&ascii_path, 10, 12).await.unwrap(),
            byte_range(13, 13)
        );
    }

    #[tokio::test]
    async fn sync_merkle_tree_dedupes_synced_nodes() {
        let store = AuggieStoreClient::new(Arc::new(MockAuggieMcpClient::new(vec![])));
        let node = node_hash("node");

        let first = store
            .sync_merkle_tree(vec![node.clone()], EmbeddingConfig::default())
            .await
            .unwrap();
        let second = store
            .sync_merkle_tree(vec![node.clone()], EmbeddingConfig::default())
            .await
            .unwrap();

        assert_eq!(first, HashSet::from([node]));
        assert!(second.is_empty());
    }

    #[tokio::test]
    async fn get_relevant_fragments_drops_excerpt_outside_repo() {
        let repo_dir = tempfile::tempdir().unwrap();
        let repo_path = dunce::canonicalize(repo_dir.path()).unwrap();
        let file_path = repo_path.join("main.rs");
        let content = "alpha\n";
        std::fs::write(&file_path, content).unwrap();
        let file_path = dunce::canonicalize(&file_path).unwrap();

        let outside_dir = tempfile::tempdir().unwrap();
        let outside_path = outside_dir.path().join("outside.rs");
        std::fs::write(&outside_path, "outside\n").unwrap();
        let outside_path = dunce::canonicalize(&outside_path).unwrap();

        let root_hash = node_hash("root");
        let store =
            AuggieStoreClient::new(Arc::new(MockAuggieMcpClient::new(vec![AuggieExcerpt {
                path: outside_path,
                line_start: 1,
                line_end: 1,
                content: "outside".to_string(),
            }])));

        store
            .generate_embeddings(
                EmbeddingConfig::default(),
                vec![test_fragment(
                    content,
                    &file_path,
                    byte_range(0, content.len()),
                )],
                root_hash.clone(),
                repo_metadata(&repo_path),
            )
            .await
            .unwrap();

        let result = store
            .get_relevant_fragments(
                EmbeddingConfig::default(),
                "query".to_string(),
                root_hash,
                repo_metadata(&repo_path),
            )
            .await
            .unwrap();

        assert!(result.is_empty());
    }
}
