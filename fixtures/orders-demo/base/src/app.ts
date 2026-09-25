import express from "express";
import { OrderController } from "./controllers/order.controller";
import { PaymentWebhookService } from "./services/payment-webhook.service";

export function buildRouter(controller: OrderController, webhooks: PaymentWebhookService) {
  const router = express.Router();
  router.post("/orders", (req, res) => controller.create(req, res));
  router.post("/orders/:id/cancel", (req, res) => controller.cancel(req, res));
  router.post("/webhooks/payment-captured", (req, res) => webhooks.onPaymentCaptured(req.body));
  return router;
}
