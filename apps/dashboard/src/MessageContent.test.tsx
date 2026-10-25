import { afterEach, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { MessageContent } from "./MessageContent";
import "./i18n";

afterEach(cleanup);

it("renders text, nested typed tool results and unknown parts without losing content", () => {
  render(
    <MessageContent
      content={[
        { type: "text", text: "Native text" },
        {
          type: "tool_result",
          content: [{ type: "input_text", text: "Tool text" }],
        },
        { type: "future_part", value: { count: 3 } },
      ]}
    />,
  );
  expect(screen.getByText("Native text")).toBeInTheDocument();
  expect(screen.getByText("Tool text")).toBeInTheDocument();
  expect(
    screen.getByText('{"type":"future_part","value":{"count":3}}'),
  ).toBeInTheDocument();
});

it("previews only raster data images and authenticated file references", () => {
  render(
    <MessageContent
      content={[
        { type: "image_url", image_url: { url: "data:image/png;base64,AQID" } },
        { type: "input_image", file_id: "file/owned" },
        {
          type: "image_url",
          image_url: { url: "https://tracker.invalid/pixel.png" },
        },
        { type: "input_image", image_url: "data:image/svg+xml;base64,AQID" },
      ]}
    />,
  );
  expect(
    screen.getAllByRole("img").map((image) => image.getAttribute("src")),
  ).toEqual(["data:image/png;base64,AQID", "/v1/files/file%2Fowned/content"]);
  expect(screen.getByText(/tracker.invalid/)).toBeInTheDocument();
  expect(screen.getByText(/svg\+xml/)).toBeInTheDocument();
});
