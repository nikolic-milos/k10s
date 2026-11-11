//! Schema documents the completion and schema suites are built from: an
//! OpenAPI document for `apps/v1` and a CRD list. They live out of line
//! because two suites share them and neither owns them.

pub const APPS_V1_DOC: &str = r##"{
      "openapi": "3.0.0",
      "components": { "schemas": {
        "io.k8s.api.apps.v1.Deployment": {
          "description": "Deployment enables declarative updates for Pods and ReplicaSets.",
          "type": "object",
          "properties": {
            "apiVersion": { "type": "string", "description": "APIVersion defines the versioned schema." },
            "kind": { "type": "string" },
            "metadata": { "allOf": [{ "$ref": "#/components/schemas/io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta" }] },
            "spec": { "allOf": [{ "$ref": "#/components/schemas/io.k8s.api.apps.v1.DeploymentSpec" }], "description": "Specification of the desired behavior of the Deployment." }
          },
          "x-kubernetes-group-version-kind": [{ "group": "apps", "version": "v1", "kind": "Deployment" }]
        },
        "io.k8s.api.apps.v1.DeploymentSpec": {
          "type": "object",
          "required": ["selector", "template"],
          "properties": {
            "replicas": { "type": "integer", "description": "Number of desired pods." },
            "paused": { "type": "boolean" },
            "selector": { "allOf": [{ "$ref": "#/components/schemas/io.k8s.apimachinery.pkg.apis.meta.v1.LabelSelector" }] },
            "template": { "allOf": [{ "$ref": "#/components/schemas/io.k8s.api.core.v1.PodTemplateSpec" }] }
          }
        },
        "io.k8s.api.core.v1.PodTemplateSpec": {
          "type": "object",
          "properties": {
            "metadata": { "allOf": [{ "$ref": "#/components/schemas/io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta" }] },
            "spec": { "allOf": [{ "$ref": "#/components/schemas/io.k8s.api.core.v1.PodSpec" }] }
          }
        },
        "io.k8s.api.core.v1.PodSpec": {
          "type": "object",
          "required": ["containers"],
          "properties": {
            "containers": { "type": "array", "items": { "$ref": "#/components/schemas/io.k8s.api.core.v1.Container" }, "description": "List of containers belonging to the pod." },
            "hostNetwork": { "type": "boolean" },
            "restartPolicy": { "type": "string", "enum": ["Always", "OnFailure", "Never"], "description": "Restart policy for all containers within the pod." }
          }
        },
        "io.k8s.api.core.v1.Container": {
          "type": "object",
          "required": ["name"],
          "properties": {
            "name": { "type": "string", "description": "Name of the container." },
            "image": { "type": "string", "description": "Container image name." },
            "imagePullPolicy": { "type": "string", "enum": ["Always", "Never", "IfNotPresent"], "description": "Image pull policy." },
            "ports": { "type": "array", "items": { "$ref": "#/components/schemas/io.k8s.api.core.v1.ContainerPort" } }
          }
        },
        "io.k8s.api.core.v1.ContainerPort": {
          "type": "object",
          "required": ["containerPort"],
          "properties": {
            "containerPort": { "type": "integer" },
            "name": { "type": "string" },
            "protocol": { "type": "string", "enum": ["TCP", "UDP", "SCTP"] }
          }
        },
        "io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta": {
          "type": "object",
          "properties": {
            "name": { "type": "string", "description": "Name must be unique within a namespace." },
            "namespace": { "type": "string" },
            "labels": { "type": "object", "additionalProperties": { "type": "string" } },
            "annotations": { "type": "object", "additionalProperties": { "type": "string" } }
          }
        },
        "io.k8s.apimachinery.pkg.apis.meta.v1.LabelSelector": {
          "type": "object",
          "properties": {
            "matchLabels": { "type": "object", "additionalProperties": { "type": "string" } }
          }
        }
      } }
    }"##;

pub const CRD_LIST: &str = r#"{
      "kind": "CustomResourceDefinitionList",
      "apiVersion": "apiextensions.k8s.io/v1",
      "items": [{
        "metadata": { "name": "widgets.example.com" },
        "spec": {
          "group": "example.com",
          "names": { "kind": "Widget", "plural": "widgets" },
          "scope": "Namespaced",
          "versions": [{
            "name": "v1",
            "served": true,
            "storage": true,
            "schema": { "openAPIV3Schema": {
              "type": "object",
              "properties": {
                "spec": {
                  "type": "object",
                  "required": ["size"],
                  "properties": {
                    "size": { "type": "integer", "description": "How many widget units." },
                    "mode": { "type": "string", "enum": ["auto", "manual"] },
                    "tint": { "type": "string", "nullable": true },
                    "labels": { "type": "object", "additionalProperties": true },
                    "sealed": {
                      "type": "object",
                      "properties": { "on": { "type": "boolean" } },
                      "additionalProperties": false
                    }
                  }
                }
              }
            } }
          }, {
            "name": "v2alpha1",
            "served": false,
            "storage": false
          }]
        }
      }]
    }"#;
