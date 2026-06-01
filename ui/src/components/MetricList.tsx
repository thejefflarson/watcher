import { Fragment, useEffect, useState } from "react";
import {
  getMetricFacet,
  getMetricHistFacet,
  listMetrics,
  type FacetResponse,
  type HistFacetResponse,
  type MetricSummary,
} from "../api";
import { formatValue } from "../format";
import { facetLabels } from "../metricLabels";
import { useControls, rangeParams } from "../timerange";
import Sparkline, { Bars } from "./Sparkline";

// A few hours of recent buckets is enough for the inline per-series glance; the
// detail page has the full range selector.
const EXPAND_HOURS = 6;

// The "recent" glyph matches the metric type: a value sparkline for gauges, a
// per-interval rate bar chart for counters, and a mini bucket distribution (the
// histogram's actual shape) for histograms.
function RecentGlyph({ m }: { m: MetricSummary }) {
  if (m.kind === "gauge") {
    return m.spark && m.spark.length >= 2 ? (
      <Sparkline values={m.spark} />
    ) : (
      <span className="muted">—</span>
    );
  }
  if (m.kind === "histogram") {
    return m.dist && m.dist.length > 1 ? (
      <Bars values={m.dist} />
    ) : (
      <span className="muted">—</span>
    );
  }
  // counter / sum: per-interval rate (Δ of the cumulative value), oldest→newest.
  if (m.spark && m.spark.length >= 2) {
    const rate: number[] = [];
    for (let i = 0; i < m.spark.length - 1; i++) {
      rate.push(Math.max(0, m.spark[i] - m.spark[i + 1]));
    }
    rate.reverse();
    return <Bars values={rate} />;
  }
  return <span className="muted">—</span>;
}

// Gauge/sum series rows: value (or per-second rate) sparkline + latest.
function FacetSeriesRows({ name, unit }: { name: string; unit: string | null }) {
  const [data, setData] = useState<FacetResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let active = true;
    getMetricFacet({ name, hours: EXPAND_HOURS })
      .then((d) => active && (setData(d), setError(null)))
      .catch((e: unknown) => active && setError(String(e)));
    return () => {
      active = false;
    };
  }, [name]);

  if (error) return <p className="error small">Failed to load: {error}</p>;
  if (!data) return <p className="muted small">Loading…</p>;
  const labels = facetLabels(data.series);
  const u = data.rated ? `${unit ?? ""}/s` : unit;
  return (
    <table className="subseries">
      <caption className="muted small subseries-count">{data.series.length} series</caption>
      <tbody>
        {data.series.map((s, i) => {
          const vals = s.points.map((p) => p.v).filter((v): v is number => v != null);
          const latest = vals.length ? vals[vals.length - 1] : null;
          return (
            <tr key={i}>
              <td className="mono sub-label">{labels[i]}</td>
              <td>
                {vals.length < 2 ? (
                  <span className="muted">—</span>
                ) : data.rated ? (
                  // counter: per-interval rate as bars (points are oldest→newest)
                  <Bars values={vals} />
                ) : (
                  <Sparkline values={[...vals].reverse()} />
                )}
              </td>
              <td className="num">{formatValue(latest, u)}</td>
            </tr>
          );
        })}
        {data.truncated > 0 && (
          <tr>
            <td className="muted small" colSpan={3}>
              +{data.truncated} more series
            </td>
          </tr>
        )}
      </tbody>
    </table>
  );
}

// Histogram series rows: latency distribution bars + latest p50/p95/p99.
function HistSeriesRows({ name, unit }: { name: string; unit: string | null }) {
  const [data, setData] = useState<HistFacetResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let active = true;
    getMetricHistFacet({ name, hours: EXPAND_HOURS })
      .then((d) => active && (setData(d), setError(null)))
      .catch((e: unknown) => active && setError(String(e)));
    return () => {
      active = false;
    };
  }, [name]);

  if (error) return <p className="error small">Failed to load: {error}</p>;
  if (!data) return <p className="muted small">Loading…</p>;
  const labels = facetLabels(data.series);
  return (
    <table className="subseries">
      <caption className="muted small subseries-count">{data.series.length} series</caption>
      <tbody>
        {data.series.map((s, i) => {
          const last = s.points.length ? s.points[s.points.length - 1] : null;
          return (
            <tr key={i}>
              <td className="mono sub-label">{labels[i]}</td>
              <td title="latency distribution">
                {s.dist && s.dist.length > 1 ? (
                  <Bars values={s.dist} />
                ) : (
                  <span className="muted">—</span>
                )}
              </td>
              <td className="num">
                {last ? (
                  <span className="pcts">
                    p50 {formatValue(last.p50, unit)} · p95 {formatValue(last.p95, unit)} · p99{" "}
                    {formatValue(last.p99, unit)}
                  </span>
                ) : (
                  "—"
                )}
              </td>
            </tr>
          );
        })}
        {data.truncated > 0 && (
          <tr>
            <td className="muted small" colSpan={3}>
              +{data.truncated} more series
            </td>
          </tr>
        )}
      </tbody>
    </table>
  );
}

function ExpandedMetric({ m }: { m: MetricSummary }) {
  return m.kind === "histogram" ? (
    <HistSeriesRows name={m.name} unit={m.unit} />
  ) : (
    <FacetSeriesRows name={m.name} unit={m.unit} />
  );
}

export default function MetricList({
  onSelect,
}: {
  onSelect: (m: MetricSummary) => void;
}) {
  const [metrics, setMetrics] = useState<MetricSummary[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loaded, setLoaded] = useState(false);
  const [service, setService] = useState("");
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const { rangeKey, tick } = useControls();

  useEffect(() => {
    let active = true;
    const handle = setTimeout(() => {
      listMetrics({ service: service || undefined, ...rangeParams(rangeKey) })
        .then((m) => {
          if (active) {
            setMetrics(m);
            setError(null);
          }
        })
        .catch((e: unknown) => active && setError(String(e)))
        .finally(() => active && setLoaded(true));
    }, 250);
    return () => {
      active = false;
      clearTimeout(handle);
    };
  }, [service, rangeKey, tick]);

  const toggle = (name: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      next.has(name) ? next.delete(name) : next.add(name);
      return next;
    });

  if (error) return <p className="error">Failed to load: {error}</p>;

  return (
    <div>
      <div className="filters">
        <input
          placeholder="service"
          value={service}
          onChange={(e) => setService(e.target.value)}
        />
      </div>
      {!loaded ? (
        <p className="muted">Loading…</p>
      ) : metrics.length === 0 ? (
        <p className="muted">No metrics yet.</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>metric</th>
              <th>kind</th>
              <th>recent</th>
              <th className="num">last</th>
            </tr>
          </thead>
          <tbody>
            {metrics.map((m) => {
              const multi = (m.series_count ?? 1) > 1;
              const open = expanded.has(m.name);
              return (
                <Fragment key={m.name}>
                  <tr className="clickable" onClick={() => onSelect(m)} title="View time series">
                    <td className="mono">
                      {multi && (
                        <button
                          className="expander"
                          onClick={(e) => {
                            e.stopPropagation();
                            toggle(m.name);
                          }}
                          title="Expand series"
                        >
                          {open ? "▾" : "▸"}
                        </button>
                      )}
                      {m.name}
                    </td>
                    <td className="muted">{m.kind ?? "—"}</td>
                    <td>
                      {multi ? <span className="muted">—</span> : <RecentGlyph m={m} />}
                    </td>
                    <td className="num">
                      {multi ? (
                        <span className="muted">—</span>
                      ) : (
                        formatValue(m.last_value, m.unit)
                      )}
                    </td>
                  </tr>
                  {open && (
                    <tr className="subseries-row">
                      <td colSpan={4}>
                        <ExpandedMetric m={m} />
                      </td>
                    </tr>
                  )}
                </Fragment>
              );
            })}
          </tbody>
        </table>
      )}
    </div>
  );
}
