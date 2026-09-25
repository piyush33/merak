import { Request, Response } from "express";
import { OrderService } from "../services/order.service";

export class OrderController {
  constructor(private readonly service: OrderService) {}

  async create(req: Request, res: Response): Promise<void> {
    const order = await this.service.createOrder(req.body.customerId, req.body.items);
    res.status(201).json(order);
  }

  async cancel(req: Request, res: Response): Promise<void> {
    await this.service.cancelOrder(req.user, req.params.id);
    res.status(204).end();
  }
}
