import { memo } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';

/**
 * 受控 Markdown 渲染：模型输出只经 react-markdown 生成 React 元素，不经 innerHTML，
 * 原始 HTML 一律不渲染（skipHtml），链接只允许 http(s) 并在新窗口打开。
 */
export const Markdown = memo(function Markdown({ text, className }: { text: string; className?: string }) {
  return (
    <div className={`prose ${className ?? ''}`}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        skipHtml
        urlTransform={(url) => (/^https?:\/\//i.test(url) ? url : '')}
        components={{
          a: ({ href, children }) =>
            href ? (
              <a href={href} target="_blank" rel="noreferrer noopener">
                {children}
              </a>
            ) : (
              <span>{children}</span>
            ),
          img: ({ alt }) => <span className="tag">图片：{alt ?? '未命名'}</span>,
        }}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
});
