# Domain

The domain module defines the engine's shared data types.

Source files remain untouched. Files are extracted into entities that carry content and its source location. Entities are indexed according to their content kind, using the workspace's configured model routing.

1. **Workspace** defines a named scope for indexing and searching source files.
1. **Glob rule** defines a pattern for matching paths.
1. **Source** describes source files and directories, their formats and locations.
1. **Content** describes content payloads and kinds.
1. **Metadata** describes category-specific attributes of content, such as a code symbol’s name and kind.
1. **Entity** combines content, source references, and metadata into a logical search unit.
1. **Model** defines model identity, configuration, and execution progress.
