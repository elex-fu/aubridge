import { useEffect, useState } from "react";

export function CopyBtn({ text, id, label = "复制" }: { text: string | null; id?: string; label?: string }) {
  const [feedback, setFeedback] = useState("");
  const pending = feedback === "复制中…";

  useEffect(() => {
    if (!feedback || pending) return;
    const timer = window.setTimeout(() => setFeedback(""), 1500);
    return () => window.clearTimeout(timer);
  }, [feedback, pending]);

  return (
    <button
      className="copy"
      disabled={text == null || pending}
      onClick={async () => {
        if (text == null || pending) return;
        setFeedback("复制中…");
        try {
          await navigator.clipboard.writeText(text);
          setFeedback("已复制");
        } catch {
          setFeedback("复制失败");
        }
      }}
      aria-label={id}
    >
      {feedback || label}
    </button>
  );
}
