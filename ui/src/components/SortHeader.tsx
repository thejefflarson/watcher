import type { Sort } from "../sort";

// A sortable column header. The `<th>` keeps its native columnheader role (and
// carries aria-sort); the click/keyboard affordance lives on a real <button>
// inside it, so sorting is keyboard-operable and gets the ink focus ring for
// free — no bare onClick on a static <th>.
export default function SortHeader<T>({
  sort,
  field,
  label,
  num = false,
}: {
  sort: Sort<T>;
  field: keyof T;
  label: string;
  num?: boolean;
}) {
  return (
    <th className={(num ? "num " : "") + "sortable"} aria-sort={sort.ariaSort(field)}>
      <button type="button" className="th-sort" onClick={() => sort.onSort(field)}>
        {label}
        {sort.indicator(field)}
      </button>
    </th>
  );
}
