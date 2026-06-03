#!/bin/sh
# LocalStack initialisation script — runs once when LocalStack becomes ready.
# Creates the S3 bucket used by Recursive's S3StorageBackend in dev mode.
set -e

echo "[localstack-init] creating recursive-dev S3 bucket"
awslocal s3 mb s3://recursive-dev --region us-east-1
echo "[localstack-init] bucket created: s3://recursive-dev"
