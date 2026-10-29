import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import App from "./app/App";
import "./app/app.css";

const rootEl = document.getElementById("root");
if (rootEl === null) {
  throw new Error("#root element not found");
}

createRoot(rootEl).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
