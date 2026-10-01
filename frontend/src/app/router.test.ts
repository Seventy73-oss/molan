import { describe, expect, it } from 'vitest';
import { navigateFrom, parseHash, routeHref } from './router';

describe('路由', () => {
  it('解析与生成互逆', () => {
    for (const h of ['#/', '#/book/a%2Fb', '#/skills', '#/skills/x', '#/sources', '#/settings/book']) {
      expect(routeHref(parseHash(h))).toBe(h);
    }
  });

  it('异步完成后的跳转只在作者仍停留原页面时生效', () => {
    location.hash = '#/';
    const from = location.hash;
    expect(navigateFrom(from, { name: 'book', bookId: 'new' })).toBe(true);
    expect(location.hash).toBe('#/book/new');
    // 操作进行中作者已去别处：不把作者拉回 / 拉走
    const started = '#/';
    location.hash = '#/book/other';
    expect(navigateFrom(started, { name: 'book', bookId: 'late' })).toBe(false);
    expect(location.hash).toBe('#/book/other');
  });
});
