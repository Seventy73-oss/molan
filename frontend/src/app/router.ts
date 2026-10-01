/**
 * Hash 路由：Rust 单端口静态托管对未知路径返回 404，用 hash 路由无需服务端改写即可刷新/直达。
 *   #/            书库
 *   #/book/:id    工作台
 *   #/skills      技能库
 *   #/sources     书源
 *   #/settings/:tab?
 */
import { useSyncExternalStore } from 'react';

export type Route =
  | { name: 'library' }
  | { name: 'book'; bookId: string }
  | { name: 'skills'; skillId?: string }
  | { name: 'sources' }
  | { name: 'settings'; tab?: string };

export function parseHash(hash: string): Route {
  const h = hash.replace(/^#\/?/, '');
  const [head, a] = h.split('/').map((x) => decodeURIComponent(x ?? ''));
  switch (head) {
    case 'book':
      return a ? { name: 'book', bookId: a } : { name: 'library' };
    case 'skills':
      return { name: 'skills', skillId: a || undefined };
    case 'sources':
      return { name: 'sources' };
    case 'settings':
      return { name: 'settings', tab: a || undefined };
    default:
      return { name: 'library' };
  }
}

export function routeHref(r: Route): string {
  switch (r.name) {
    case 'library':
      return '#/';
    case 'book':
      return `#/book/${encodeURIComponent(r.bookId)}`;
    case 'skills':
      return r.skillId ? `#/skills/${encodeURIComponent(r.skillId)}` : '#/skills';
    case 'sources':
      return '#/sources';
    case 'settings':
      return r.tab ? `#/settings/${r.tab}` : '#/settings';
  }
}

export function navigate(r: Route) {
  const href = routeHref(r);
  if (location.hash !== href) location.hash = href;
}

/**
 * 异步操作完成后的跳转：只有作者仍停留在发起操作时的页面（hash 未变）才跳转，
 * 绝不把已经离开的作者拉回来（例如创建作品仍在进行时作者已打开了另一部作品）。
 */
export function navigateFrom(startHash: string, r: Route): boolean {
  if (location.hash !== startHash) return false;
  navigate(r);
  return true;
}

function subscribe(cb: () => void) {
  window.addEventListener('hashchange', cb);
  return () => window.removeEventListener('hashchange', cb);
}

export function useRoute(): Route {
  const hash = useSyncExternalStore(subscribe, () => location.hash, () => '');
  return parseHash(hash);
}
