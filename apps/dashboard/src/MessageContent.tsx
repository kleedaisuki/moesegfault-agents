import { useTranslation } from "react-i18next";
import type { MessageContent as Content } from "./api";

export function MessageContent({ content }: { content: Content }) {
  const { t } = useTranslation();
  if (Array.isArray(content))
    return content.map((part, index) => (
      <MessageContent key={index} content={part} />
    ));
  if (content === null) return null;
  if (typeof content !== "object") return <p>{String(content)}</p>;
  if (
    ["text", "input_text", "output_text"].includes(String(content.type)) &&
    typeof content.text === "string"
  )
    return <p>{content.text}</p>;
  if (content.type === "tool_result" && content.content !== undefined)
    return <MessageContent content={content.content} />;
  if (content.type === "image_url" || content.type === "input_image") {
    const image = content.image_url;
    const url =
      typeof image === "string"
        ? image
        : image && !Array.isArray(image) && typeof image === "object"
          ? image.url
          : null;
    // Journal data must not cause remote requests or render active SVG content.
    const source =
      typeof content.file_id === "string"
        ? `/v1/files/${encodeURIComponent(content.file_id)}/content`
        : typeof url === "string" &&
            /^data:image\/(png|jpeg|gif|webp);base64,[A-Za-z0-9+/]+=*$/.test(
              url,
            )
          ? url
          : null;
    return source ? (
      <img className="message-image" src={source} alt={t("Attached image")} />
    ) : (
      <p>{JSON.stringify(content)}</p>
    );
  }
  return <p>{JSON.stringify(content)}</p>;
}
