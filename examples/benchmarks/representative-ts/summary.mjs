import { pageCount } from './pages.ts';

export function summary(rows) {
    return `${pageCount(rows)} pages`;
}
