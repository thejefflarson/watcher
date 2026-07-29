import React from "react";
import ReactDOM from "react-dom/client";
import { BrowserRouter } from "react-router";
import App from "./App";
import { TimeRangeProvider } from "./timerange";
import "./styles.css";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <BrowserRouter>
      <TimeRangeProvider>
        <App />
      </TimeRangeProvider>
    </BrowserRouter>
  </React.StrictMode>,
);
