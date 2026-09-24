import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./app.css";
import { applyTheme, cachedTheme } from "./theme";

// 尽早应用主题(profile.json 是异步读取的,这里先用本机缓存避免闪屏)
applyTheme(cachedTheme());

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
