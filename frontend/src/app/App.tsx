import { lazy, Suspense, useEffect, useState } from 'react';
import { Icon } from '../components/Icon';
import { Spinner } from '../components/Status';
import { ToastStack } from '../components/Toasts';
import { api } from '../lib/api';
import { CONTRACT_VERSION, type AppInfo } from '../lib/contracts';
import { errorText } from '../lib/ipc';
import { LibraryPage } from '../features/library/LibraryPage';
import { WorkspacePage } from '../features/workspace/WorkspacePage';
import { applyEditorVars, applyTheme, usePrefs, type ThemePref } from './prefs';
import { navigate, routeHref, useRoute, type Route } from './router';

const SkillsPage = lazy(() => import('../features/skills/SkillsPage'));
const SettingsPage = lazy(() => import('../features/settings/SettingsPage'));
const SourcesPage = lazy(() => import('../features/sources/SourcesPage'));

const NAV: { route: Route; icon: string; label: string }[] = [
  { route: { name: 'library' }, icon: 'books', label: '书库' },
  { route: { name: 'skills' }, icon: 'layers', label: '技能' },
  { route: { name: 'sources' }, icon: 'globe', label: '书源' },
  { route: { name: 'settings' }, icon: 'settings', label: '设置' },
];

const THEME_NEXT: Record<ThemePref, ThemePref> = { system: 'light', light: 'dark', dark: 'system' };
const THEME_ICON: Record<ThemePref, string> = { system: 'monitor', light: 'sun', dark: 'moon' };
const THEME_LABEL: Record<ThemePref, string> = { system: '跟随系统', light: '浅色', dark: '暗色' };

export function App() {
  const route = useRoute();
  const prefs = usePrefs();
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [bootError, setBootError] = useState<string | null>(null);

  useEffect(() => {
    applyTheme(prefs.theme);
    applyEditorVars(prefs);
    api
      .appInfo()
      .then((i) => {
        setInfo(i);
        if (i.contract !== CONTRACT_VERSION) setBootError(`前后端契约版本不一致（服务端 v${i.contract}，前端 v${CONTRACT_VERSION}），请重新构建前端或回退服务端`);
      })
      .catch((e) => setBootError(errorText(e)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const active = (r: Route) => r.name === route.name || (r.name === 'library' && route.name === 'book');

  return (
    <div className={`app${route.name === 'book' ? ' app--workspace' : ''}`}>
      <nav className="rail" aria-label="主导航">
        <a className="rail__brand" href="#/" aria-label="墨澜工坊 书库">
          <span className="rail__logo">墨</span>
        </a>
        {NAV.map((n) => (
          <a key={n.label} href={routeHref(n.route)} className={`rail__item${active(n.route) ? ' rail__item--active' : ''}`} aria-current={active(n.route) ? 'page' : undefined}>
            <Icon name={n.icon} size={20} />
            <span className="rail__label">{n.label}</span>
          </a>
        ))}
        <span className="rail__spacer" />
        <button className="rail__item" onClick={() => prefs.set({ theme: THEME_NEXT[prefs.theme] })} aria-label={`主题：${THEME_LABEL[prefs.theme]}（点击切换）`} title={`主题：${THEME_LABEL[prefs.theme]}`}>
          <Icon name={THEME_ICON[prefs.theme]} size={20} />
          <span className="rail__label">{THEME_LABEL[prefs.theme]}</span>
        </button>
      </nav>
      <main className="main">
        {bootError ? (
          <div className="boot-error notice notice--bad" role="alert">
            <Icon name="alert" />
            <div className="stack stack--tight">
              <strong>无法连接或契约不匹配</strong>
              <span className="break">{bootError}</span>
              <button className="btn btn--sm" onClick={() => location.reload()}>
                重试
              </button>
            </div>
          </div>
        ) : null}
        <Suspense fallback={<div className="page-loading"><Spinner label="加载中" /></div>}>
          {route.name === 'library' ? <LibraryPage /> : null}
          {route.name === 'book' ? <WorkspacePage bookId={route.bookId} info={info} /> : null}
          {route.name === 'skills' ? <SkillsPage skillId={route.skillId} /> : null}
          {route.name === 'sources' ? <SourcesPage /> : null}
          {route.name === 'settings' ? <SettingsPage tab={route.tab} onNavigate={(tab) => navigate({ name: 'settings', tab })} /> : null}
        </Suspense>
      </main>
      <ToastStack />
    </div>
  );
}
