ECR_REGISTRY=667512625118.dkr.ecr.eu-north-1.amazonaws.com
IMAGE_TAG=v0.2.3

aws login --profile opentiles-deploy

aws ecr get-login-password \
  --region eu-north-1 \
  --profile opentiles-deploy |
docker login \
  --username AWS \
  --password-stdin "$ECR_REGISTRY"


docker buildx build \
  --platform linux/arm64 \
  --load \
  --progress=plain \
  -t "opentiles-server:$IMAGE_TAG" \
  .


docker tag \
  "opentiles-server:$IMAGE_TAG" \
  "$ECR_REGISTRY/opentiles-server:$IMAGE_TAG"

  docker push "$ECR_REGISTRY/opentiles-server:$IMAGE_TAG"