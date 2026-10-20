type BrowserIconName = "back" | "forward" | "home" | "activity" | "settings";

/** One angular, circuit-inspired icon family for the browser toolbar. */
export function BrowserIcon({ name }: { name: BrowserIconName }) {
  return <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="square" strokeLinejoin="miter" aria-hidden="true" focusable="false">
    {name === "back" && <><path d="m10 6-6 6 6 6M4 12h15v-3" /><path opacity=".45" d="M4 4h4M20 20h-4" /></>}
    {name === "forward" && <><path d="m14 6 6 6-6 6M20 12H5v3" /><path opacity=".45" d="M20 4h-4M4 20h4" /></>}
    {name === "home" && <><path d="m3 11 9-8 9 8M5 10v10h5v-6h4v6h5V10" /><path opacity=".45" d="M2 17v4h4M22 17v4h-4M10 8h4" /></>}
    {name === "activity" && <><path d="M3 8V4h5M16 4h5v4M21 16v4h-5M8 20H3v-4" /><path d="M3 12h4l2-5 4 10 2-5h6" /><path opacity=".45" d="M11 4h2M11 20h2" /></>}
    {name === "settings" && <><path d="m9 3-1 3-3 1-2 3 2 2-1 3 2 3h3l2 3h3l1-3 3-1 2-3-2-2 1-3-2-3h-3l-2-3Z" /><path d="m9 10 3-2 3 2v4l-3 2-3-2Z" /><path opacity=".45" d="M12 1v2M21 11h2M1 13h2M12 21v2" /></>}
  </svg>;
}
