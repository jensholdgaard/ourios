#!/usr/bin/env python3
"""Render the Ourios Helm chart topology (RFC 0019, S3-compatible object storage).

Generates `topology.png` next to this file using the mingrammer `diagrams`
library (https://diagrams.mingrammer.com/) and its Kubernetes node set
(https://diagrams.mingrammer.com/docs/nodes/k8s). Requires Graphviz (`dot`)
and the `diagrams` package:

    python3 -m venv .venv && . .venv/bin/activate
    pip install diagrams        # plus a system Graphviz install
    python deploy/helm/ourios/docs/topology.py

This diagram lives with the chart (outside the `docs/` mdBook tree), so the
mdBook diagram conventions (`CLAUDE.md` §6.7: Mermaid for RFCs, SVG for
lectures) do not apply; the chart README embeds the rendered PNG. Keep this
script in sync with the chart when the topology changes.
"""

from pathlib import Path

from diagrams import Cluster, Diagram, Edge

# Kubernetes node icons — https://diagrams.mingrammer.com/docs/nodes/k8s
from diagrams.k8s.compute import Deployment, Pod, StatefulSet
from diagrams.k8s.ecosystem import Helm
from diagrams.k8s.group import Namespace
from diagrams.k8s.network import Service
from diagrams.k8s.podconfig import Secret
from diagrams.k8s.rbac import ServiceAccount
from diagrams.k8s.storage import PVC, StorageClass

# The store is external object storage (the S3 icon is the recognizable
# object-store glyph; the backend is any S3-compatible provider, not AWS-only).
# Clients are off-cluster.
from diagrams.aws.storage import S3
from diagrams.onprem.client import Client

OUTFILE = Path(__file__).with_suffix("")  # .../topology -> diagrams appends .png

graph_attr = {"fontsize": "16", "labelloc": "t", "pad": "0.5", "splines": "spline"}

with Diagram(
    "Ourios Helm chart — S3-compatible object-storage topology (RFC 0019)",
    filename=str(OUTFILE),
    show=False,
    direction="TB",
    outformat="png",
    graph_attr=graph_attr,
):
    otlp = Client("OTLP exporters")
    user = Client("query clients")
    chart = Helm("ourios chart")

    with Cluster("Kubernetes namespace"):
        ns = Namespace("namespace")
        sa = ServiceAccount("serviceAccount\n(IRSA role-arn, AWS EKS)")
        secret = Secret("storage.s3.existingSecret\n(OURIOS_S3_* keys)")

        with Cluster("receiver  (StatefulSet)"):
            rcv_svc = Service("receiver Service\ngRPC 4317 / HTTP 4318")
            rcv = StatefulSet("ourios-server\nreceiver (compaction off)")
            # Per-replica durable WAL — local PVC, NEVER S3 (§3.4 / §3.6).
            wal_sc = StorageClass("local class")
            wal = PVC("WAL PVC\n(per replica, local)")
            rcv_svc >> rcv
            rcv >> Edge(label="WAL-before-ack", style="bold") >> wal
            wal_sc >> Edge(style="dotted") >> wal

        with Cluster("querier  (Deployment, N replicas)"):
            qry_svc = Service("querier Service\nHTTP 4319")
            qry = [
                Deployment("ourios-server\nquerier (compaction off)"),
                Pod("…"),
            ]
            qry_svc >> qry[0]

        with Cluster("compactor  (Deployment, 1 replica)"):
            cmp = Deployment("ourios-server\ncompactor\n(sole sweeper)")

    s3 = S3("object store (S3 API)\ndata / audit / manifest")

    # The chart renders the namespaced objects.
    chart >> Edge(style="dotted", color="gray") >> ns

    # Ingress edges.
    otlp >> Edge(label="logs") >> rcv_svc
    user >> Edge(label="logs DSL") >> qry_svc

    # Data paths to the shared object store. The receiver and querier set
    # OURIOS_COMPACTION_ENABLED=0, so ONLY the dedicated compactor sweeps
    # (RFC 0009 §3.2) — no "also sweeps" edges from receiver/querier.
    rcv >> Edge(label="write Parquet", color="darkgreen") >> s3
    qry[0] >> Edge(label="read", color="blue") >> s3
    cmp >> Edge(label="sweep / compact", color="firebrick") >> s3

    # Credentials are consumed by the workloads (the pods authenticate to the
    # store), one of two mutually-exclusive ways: a mounted Secret of S3-named
    # keys (envFrom) OR, on AWS EKS, IRSA on the ServiceAccount. The pods then use
    # them on the existing workload→store paths above.
    for workload in (rcv, qry[0], cmp):
        secret >> Edge(style="dotted", label="envFrom") >> workload
        sa >> Edge(style="dotted", label="IRSA (AWS EKS)") >> workload
