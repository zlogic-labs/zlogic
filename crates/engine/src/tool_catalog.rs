use async_trait::async_trait;

use zlogic_protocol::query::{ApiResult, ToolInfo};
use zlogic_tools::ToolRegistry;

use crate::service::ToolCatalogService;

pub struct ToolCatalog {
    registry: ToolRegistry,
}

impl ToolCatalog {
    pub fn new(registry: ToolRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl ToolCatalogService for ToolCatalog {
    async fn list(&self) -> ApiResult<Vec<ToolInfo>> {
        Ok(self
            .registry
            .names()
            .into_iter()
            .filter_map(|name| self.registry.get(&name))
            .map(|tool| {
                let meta = tool.meta();
                let description = tool.definition().description;
                ToolInfo {
                    name: meta.name,
                    description: first_line(&description),
                    source: meta.source.to_string(),
                    available: tool.available(),
                }
            })
            .collect())
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_catalog_lists_every_registered_tool_in_order() {
        let registry = ToolRegistry::with_builtins();
        let expected = registry.names();
        let catalog = ToolCatalog::new(registry);

        let listed = catalog.list().await.unwrap();
        let names: Vec<_> = listed.iter().map(|t| t.name.clone()).collect();
        assert_eq!(names, expected);
        for tool in &listed {
            assert!(
                !tool.description.is_empty(),
                "{} has no description",
                tool.name
            );
            assert!(
                !tool.description.contains('\n'),
                "{} description must not contain a newline",
                tool.name
            );
            assert_eq!(tool.source, "builtin");
        }
        for required in zlogic_protocol::chat_workspace_tools() {
            assert!(
                names.contains(&required),
                "{required} is not in the catalog"
            );
        }
    }
}
