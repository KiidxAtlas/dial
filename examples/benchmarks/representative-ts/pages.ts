import { DEFAULT_PAGE_SIZE } from './settings.ts';

export function pageCount(rows: readonly unknown[] | null): number {
    return Math.ceil(rows!.length / DEFAULT_PAGE_SIZE);
}
