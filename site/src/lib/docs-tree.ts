import type { Node as TreeNode } from 'fumadocs-core/page-tree';

export type FolderNode = Extract<TreeNode, { type: 'folder' }>;
export type PageNode = Extract<TreeNode, { type: 'page' }>;

export function explicitIndexChild(node: FolderNode): PageNode | undefined {
  return node.children.find(
    (child): child is PageNode =>
      child.type === 'page' && /(^|\/)index\.mdx?$/.test(child.$ref ?? ''),
  );
}

/* Package meta-manager is a conceptual section whose existing pages intentionally
   keep their short public URLs. Group those root nodes into one navigation folder
   without coupling the URL structure to the content-file structure. */
export function groupPackageMetaManager(nodes: TreeNode[]): TreeNode[] {
  const index = nodes.find(
    (node): node is PageNode => node.type === 'page' && node.url === '/docs/pm',
  );
  const shims = nodes.find(
    (node): node is PageNode => node.type === 'page' && node.url === '/docs/pm-shim',
  );

  if (!index || !shims) return nodes;

  const grouped: FolderNode = {
    type: 'folder',
    $id: 'package-meta-manager',
    name: index.name,
    description: index.description,
    icon: index.icon,
    index,
    children: [shims],
  };

  return nodes.flatMap((node) => {
    if (node === index) return [grouped];
    if (node === shims) return [];
    return [node];
  });
}
