import { source } from '@/lib/source';
import { groupPackageMetaManager } from '@/lib/docs-tree';
import { createFromSource } from 'fumadocs-core/search/server';

const searchSource: typeof source = {
  ...source,
  getPageTree(locale) {
    const tree = source.getPageTree(locale);
    return {
      ...tree,
      children: groupPackageMetaManager(tree.children),
    };
  },
};

export const { GET } = createFromSource(searchSource);
