export interface Page {
  id: number;
  title: string;
  tags: string[];
}

export function normalizeTitle(title: string): string {
  const trimmed = title.trim();
  if (trimmed.length === 0) {
    return "untitled";
  }
  return trimmed.toLowerCase();
}

export function tagIndex(pages: Page[]): Map<string, number[]> {
  const index = new Map<string, number[]>();
  for (const page of pages) {
    for (const tag of page.tags) {
      const ids = index.get(tag) ?? [];
      ids.push(page.id);
      index.set(tag, ids);
    }
  }
  return index;
}

export function titleOf(pages: Page[], id: number): string {
  for (const page of pages) {
    if (page.id === id) {
      return normalizeTitle(page.title);
    }
  }
  return "untitled";
}

export function titles(pages: Page[]): string[] {
  const result: string[] = [];
  for (const page of pages) {
    result.push(normalizeTitle(page.title));
  }
  return result;
}
