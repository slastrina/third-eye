import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

export interface MarkdownProps {
  /** The markdown source — an assistant message's streamed text. */
  text: string;
}

/** Assistant-message markdown: GFM (bold, lists, tables, code, strikethrough)
 *  rendered safely — react-markdown never injects raw HTML, so model output
 *  cannot script the overlay. Links open nowhere by default (a navigation
 *  would tear down the overlay webview mid-chat); they render as styled text
 *  with the href in the tooltip so the user can read the destination. */
export function Markdown({ text }: MarkdownProps) {
  return (
    <ReactMarkdown
      remarkPlugins={[remarkGfm]}
      components={{
        a: ({ href, children }) => (
          <a
            href={href}
            title={href}
            onClick={(event) => event.preventDefault()}
            rel="noopener noreferrer"
          >
            {children}
          </a>
        ),
      }}
    >
      {text}
    </ReactMarkdown>
  );
}
