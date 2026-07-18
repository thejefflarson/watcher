import { useMemo, useState } from "react";

// Client-side column sort for a table. Numbers sort numerically, nulls last,
// everything else by locale string. Clicking the active column flips direction.
export function useSort<T>(rows: T[], initial: keyof T, initialDir: "asc" | "desc" = "desc") {
  const [key, setKey] = useState<keyof T>(initial);
  const [dir, setDir] = useState<"asc" | "desc">(initialDir);

  const sorted = useMemo(() => {
    const out = [...rows];
    out.sort((a, b) => {
      const av = a[key] as unknown;
      const bv = b[key] as unknown;
      let c: number;
      if (av == null) c = bv == null ? 0 : 1;
      else if (bv == null) c = -1;
      else if (typeof av === "number" && typeof bv === "number") c = av - bv;
      else c = String(av).localeCompare(String(bv));
      return dir === "asc" ? c : -c;
    });
    return out;
  }, [rows, key, dir]);

  const onSort = (k: keyof T) => {
    if (k === key) setDir((d) => (d === "asc" ? "desc" : "asc"));
    else {
      setKey(k);
      setDir("desc");
    }
  };

  const indicator = (k: keyof T) => (k === key ? (dir === "asc" ? " ▲" : " ▼") : "");

  // Reflect the sort state to assistive tech on the active column header.
  const ariaSort = (k: keyof T): "ascending" | "descending" | "none" =>
    k === key ? (dir === "asc" ? "ascending" : "descending") : "none";

  return { sorted, onSort, indicator, ariaSort };
}

export type Sort<T> = ReturnType<typeof useSort<T>>;
